/**
 * src/engine.js — Headless per-torrent download engine.
 *
 * The same peer/piece logic the CLI uses, but event-driven and UI-free:
 * each torrent gets an engine instance that pushes status snapshots through
 * `onUpdate` and can be cancelled. The web server drives this directly;
 * any other front end can too.
 */

import net from 'net';
import crypto from 'crypto';
import { getPeers } from './tracker.js';
import { buildHandshake, buildInterested, buildRequest, parseMessage } from './Message.js';
import Pieces from './Pieces.js';
import FileManager from './FileManager.js';
import Seeder from './Seeder.js';

const BLOCK_SIZE = 2 ** 14;       // 16 KiB
const MAX_ACTIVE = 10;
const CLIENT_PORT = 6881;
const INVALID_IPS = new Set(['0.0.0.0', '127.0.0.1', '255.255.255.255']);

function verifyPiece(torrent, pieceIndex, buf) {
    const expected = torrent.info.pieces.slice(pieceIndex * 20, pieceIndex * 20 + 20);
    const actual = crypto.createHash('sha1').update(buf).digest();
    return actual.equals(expected);
}

function sleep(ms, cancelled) {
    return new Promise(resolve => {
        const t = setTimeout(resolve, ms);
        cancelled.fns.push(() => { clearTimeout(t); resolve(); });
    });
}

/**
 * Start downloading a torrent into outputDir.
 * Returns { promise, cancel } ; snapshots are pushed to onUpdate.
 */
export default function startTorrentDownload({ torrent, outputDir, onUpdate = () => {} }) {
    const cancelled = { value: false, fns: [] };
    let fileManager = null;
    let tickerRef = null;
    let seeder = null;

    const promise = (async () => {
        try {
            const info = torrent.info;
            const numPieces = Math.ceil(info.pieces.length / 20);
            fileManager = new FileManager(torrent, outputDir);

            const state = {
                status: 'checking',
                progress: 0,
                downSpeed: 0,
                upSpeed: 0,
                peers: 0,
                seedPeers: 0,
                verified: 0,
                failed: 0,
                eta: null,
                downloaded: 0,
                uploaded: 0,
            };

            const emit = (patch = {}) => {
                Object.assign(state, patch);
                try {
                    onUpdate({ ...state, cancelled: false });
                } catch (_) {}
            };
            emit();

            // ── resume check ───────────────────────────────────────────────
            const piecesState = new Pieces(numPieces);
            for (let i = 0; i < numPieces && !cancelled.value; i++) {
                const buf = fileManager.readPieceForVerify(i, numPieces);
                if (buf && verifyPiece(torrent, i, buf)) {
                    piecesState.addReceived(i);
                    state.verified++;
                    state.progress = Math.round((state.verified / numPieces) * 100);
                }
                if (i % 100 === 0) emit();
            }
            emit({ status: piecesState.isDone() ? 'seeding' : 'downloading' });

            let uploaded = 0;
            let lastDownBytes = 0, lastUpBytes = 0, lastTick = Date.now();
            let downBps = 0, upBps = 0;

            const ticker = setInterval(() => {
                const now = Date.now();
                const elapsed = (now - lastTick) / 1000;
                if (elapsed >= 1) {
                    downBps = (state.downloaded - lastDownBytes) / elapsed;
                    upBps = (uploaded - lastUpBytes) / elapsed;
                    lastDownBytes = state.downloaded;
                    lastUpBytes = uploaded;
                    lastTick = now;
                }
                emit({
                    downSpeed: downBps,
                    upSpeed: upBps,
                    eta: downBps > 0
                        ? Math.floor(((numPieces - state.verified) * torrent.info['piece length']) / downBps)
                        : null,
                    seedPeers: seeder?.connections.size ?? 0,
                });
            }, 1000);
            tickerRef = ticker;
            cancelled.fns.push(() => clearInterval(ticker));

            if (piecesState.isDone()) {
                emit({ status: 'seeding', progress: 100, eta: null });
                seeder = new Seeder(torrent, fileManager, piecesState, CLIENT_PORT);
                seeder.start();
                return { done: true };
            }

            // Concurrent seeding while downloading (mirrors download.js)
            seeder = new Seeder(torrent, fileManager, piecesState, CLIENT_PORT);
            seeder.onUpload = bytes => { uploaded += bytes; };
            seeder.start();

            // ── announce + peer loop (mirrors download.js) ────────────────
            let attempt = 0;
            while (!piecesState.isDone() && !cancelled.value) {
                attempt++;
                const peers = await new Promise(resolve => {
                    getPeers(torrent, CLIENT_PORT, list => resolve(list));
                    cancelled.fns.push(() => resolve([]));
                });
                if (cancelled.value) break;

                const valid = (peers ?? []).filter(p =>
                    p && typeof p.port === 'number' &&
                    p.port > 0 && p.port < 65536 &&
                    !INVALID_IPS.has(p.ip));

                if (valid.length === 0) {
                    await sleep(15000, cancelled);
                    continue;
                }

                await runPeerBatch(valid, torrent, piecesState, fileManager, numPieces, state, torrent.info.files
                    ? torrent.info.files.reduce((s, f) => s + f.length, 0)
                    : torrent.info.length, seeder, onUpdate, {
                    addDownloaded: n => { state.downloaded += n; },
                    addFailed: () => { state.failed++; },
                });
                if (cancelled.value) break;
                if (!piecesState.isDone()) await sleep(10000, cancelled);
            }

            if (!cancelled.value) {
                fileManager.close();
                emit({ status: 'seeding', progress: 100, eta: null, downSpeed: 0, peers: 0 });
            }
            return { done: piecesState.isDone() };
        } catch (err) {
            onUpdate({ status: 'error', error: err.message });
            return { done: false, error: err.message };
        }
    })();

    function cancel() {
        cancelled.value = true;
        for (const fn of cancelled.fns.splice(0)) fn();
        seeder?.stop();
        try { fileManager?.close(); } catch (_) {}
    }

    return { promise, cancel };
}

// ── peer batch (same structure as download.js) ─────────────────────────────
function runPeerBatch(peers, torrent, pieces, fileManager, numPieces, state,
                      totalSize, seeder, onUpdate, hooks) {
    return new Promise(resolve => {
        let peerIndex = 0;
        let active = 0;

        function spawnNext() {
            if (pieces.isDone()) { if (active === 0) resolve(); return; }
            while (active < MAX_ACTIVE && peerIndex < peers.length) {
                connectToPeer(peers[peerIndex++]);
                active++;
                if (state) state.peers = active;
            }
            if (peerIndex >= peers.length && active === 0) resolve();
        }

        function done() {
            active--;
            if (state) state.peers = Math.max(0, active);
            if (pieces.isDone()) {
                if (active === 0) resolve();
            } else {
                spawnNext();
                if (peerIndex >= peers.length && active === 0) resolve();
            }
        }

        function connectToPeer(peer) {
            const socket = new net.Socket();
            let closed = false;

            socket.setTimeout(15000);
            socket.on('timeout', close);
            socket.on('error', close);
            socket.on('close', close);
            socket.connect(peer.port, peer.ip, () => {
                socket.write(buildHandshake(torrent));
                setupMessageHandler(socket, torrent, pieces, fileManager,
                    numPieces, totalSize, state, seeder, hooks, close);
            });

            function close() {
                if (closed) return;
                closed = true;
                try { socket.destroy(); } catch (_) {}
                done();
            }
        }

        spawnNext();
    });
}

function setupMessageHandler(socket, torrent, pieces, fileManager, numPieces,
                             totalSize, state, seeder, hooks, close) {
    let buffer = Buffer.alloc(0);
    let handshake = true;
    const queue = { choked: true, entries: [] };
    const pieceBuffers = {};

    socket.on('data', data => {
        buffer = Buffer.concat([buffer, data]);
        processBuffer();
    });

    function processBuffer() {
        try {
            if (buffer.length === 0) return;
            const length = handshake
                ? buffer[0] + 49
                : buffer.readUInt32BE(0) + 4;
            if (length <= 0 || length > 131072) { buffer = Buffer.alloc(0); return; }
            if (buffer.length < length) return;
            const msg = buffer.subarray(0, length);
            buffer = buffer.subarray(length);
            handshake = false;
            handleMessage(msg);
            processBuffer();
        } catch {
            close();
        }
    }

    function handleMessage(msg) {
        if (msg.length >= 20 &&
            msg.length === msg.readUInt8(0) + 49 &&
            msg.toString('utf8', 1, 20) === 'BitTorrent protocol') {
            socket.write(buildInterested());
            return;
        }
        if (msg.length < 4) return;
        const { id, payload } = parseMessage(msg);
        if (id === 0) {
            queue.choked = true;
            close();
        } else if (id === 1) {
            queue.choked = false;
            requestPiece();
        } else if (id === 4) {
            have(payload);
        } else if (id === 5) {
            bitfield(payload);
        } else if (id === 7) {
            piece(payload);
        }
    }

    function have(payload) {
        if (!payload || payload.length < 4) return;
        const pi = payload.readUInt32BE(0);
        if (pi >= 0 && pi < numPieces) {
            queue.entries.push(pi);
            if (queue.entries.length === 1) requestPiece();
        }
    }

    function bitfield(payload) {
        if (!payload) return;
        for (let i = 0; i < payload.length; i++) {
            for (let j = 0; j < 8; j++) {
                const pi = i * 8 + j;
                if (pi < numPieces && (payload[i] >> (7 - j)) & 1) {
                    queue.entries.push(pi);
                }
            }
        }
        requestPiece();
    }

    function piece(payload) {
        if (!payload?.block) return;
        const pi = payload.index;
        const pieceLength = torrent.info['piece length'];
        const pieceTotal = pi === numPieces - 1
            ? totalSize % pieceLength || pieceLength
            : pieceLength;

        if (!pieceBuffers[pi]) pieceBuffers[pi] = Buffer.alloc(pieceTotal);
        payload.block.copy(pieceBuffers[pi], payload.begin);
        if (payload.begin + payload.block.length < pieceTotal) return;

        const pieceBuf = pieceBuffers[pi];
        delete pieceBuffers[pi];

        const expected = torrent.info.pieces.slice(pi * 20, pi * 20 + 20);
        const actual = crypto.createHash('sha1').update(pieceBuf).digest();
        if (actual.equals(expected)) {
            try {
                fileManager.writePiece(pi, pieceBuf);
                pieces.addReceived(pi);
                seeder?.announceHave(pi);
                hooks.addDownloaded(pieceBuf.length);
            } catch {
                close();
            }
        } else {
            pieces.requested.delete(pi);
            queue.entries.push(pi);
            hooks.addFailed();
        }
        requestPiece();
    }

    function requestPiece() {
        if (queue.choked) return;
        while (queue.entries.length) {
            const pi = queue.entries[0];
            if (pieces.needed(pi)) {
                const pieceLength = torrent.info['piece length'];
                const pieceTotal = pi === numPieces - 1
                    ? totalSize % pieceLength || pieceLength
                    : pieceLength;
                for (let begin = 0; begin < pieceTotal; begin += BLOCK_SIZE) {
                    try {
                        socket.write(buildRequest({
                            index: pi,
                            begin,
                            length: Math.min(BLOCK_SIZE, pieceTotal - begin),
                        }));
                    } catch (_) { return; }
                }
                pieces.addRequested(pi);
                queue.entries.shift();
                break;
            }
            queue.entries.shift();
        }
    }
}
