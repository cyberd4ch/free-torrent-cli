import net from 'net';
import crypto from 'crypto';
import cliProgress from 'cli-progress';
import chalk from 'chalk';
import { getPeers } from './tracker.js';
import { buildHandshake, buildInterested, buildRequest, parseMessage } from './Message.js';
import Pieces from './Pieces.js';
import FileManager from './FileManager.js';
import Seeder from './Seeder.js';

const BLOCK_SIZE = 2 ** 14;       // 16 KiB
const MAX_ACTIVE = 10;
const CLIENT_PORT = 6881;
const INVALID_IPS = new Set(['0.0.0.0', '127.0.0.1', '255.255.255.255']);

// ── Piece verification ────────────────────────────────────────────────────
function verifyPiece(torrent, pieceIndex, buf) {
    const expected = torrent.info.pieces.slice(pieceIndex * 20, pieceIndex * 20 + 20);
    const actual = crypto.createHash('sha1').update(buf).digest();
    return actual.equals(expected);
}

// ── Resume: verify pieces already on disk via FileManager ─────────────────
function buildResumeState(torrent, fileManager, numPieces) {
    const pieces = new Pieces(numPieces);
    let resumed = 0;

    for (let i = 0; i < numPieces; i++) {
        const buf = fileManager.readPieceForVerify(i, numPieces);
        if (buf && verifyPiece(torrent, i, buf)) {
            pieces.addReceived(i);
            resumed++;
        }
    }

    return { pieces, resumed };
}

// ── Stats ─────────────────────────────────────────────────────────────────
class Stats {
    constructor() {
        this.downloaded = 0;
        this.uploaded = 0;
        this.startTime = Date.now();
        this.lastDownBytes = 0;
        this.lastUpBytes = 0;
        this.lastTime = Date.now();
        this.downBps = 0;
        this.upBps = 0;
    }

    addDown(bytes) {
        this.downloaded += bytes;
        this._tick();
    }

    addUp(bytes) {
        this.uploaded += bytes;
        this._tick();
    }

    _tick() {
        const now = Date.now();
        const elapsed = (now - this.lastTime) / 1000;
        if (elapsed >= 1) {
            this.downBps = (this.downloaded - this.lastDownBytes) / elapsed;
            this.upBps = (this.uploaded - this.lastUpBytes) / elapsed;
            this.lastDownBytes = this.downloaded;
            this.lastUpBytes = this.uploaded;
            this.lastTime = now;
        }
    }

    formatSpeed(bps) {
        if (bps > 1e6) return `${(bps / 1e6).toFixed(1)} MB/s`;
        if (bps > 1e3) return `${(bps / 1e3).toFixed(1)} KB/s`;
        return `${bps.toFixed(0)} B/s`;
    }

    downSpeed() { return this.formatSpeed(this.downBps); }
    upSpeed()   { return this.formatSpeed(this.upBps); }

    eta(remainingBytes) {
        if (this.downBps <= 0) return '--:--';
        const secs = Math.floor(remainingBytes / this.downBps);
        const m = Math.floor(secs / 60);
        const s = secs % 60;
        return `${m}m ${s.toString().padStart(2, '0')}s`;
    }

    elapsed() {
        const secs = Math.floor((Date.now() - this.startTime) / 1000);
        const m = Math.floor(secs / 60);
        const s = secs % 60;
        return `${m}m ${s.toString().padStart(2, '0')}s`;
    }
}

// ── Main ──────────────────────────────────────────────────────────────────
export default async function downloadTorrent(torrent, outputDir = process.cwd()) {
    try {
        const torrentName = torrent.info.name.toString('utf8');
        const isMultiFile = !!torrent.info.files;
        const numPieces = Math.ceil(torrent.info.pieces.length / 20);

        // FileManager handles single + multi-file layout transparently
        const fileManager = new FileManager(torrent, outputDir);
        const totalSize = fileManager.totalSize;

        // ── Header ────────────────────────────────────────────────────────
        console.log('');
        console.log(chalk.bold.cyan('  free-torrent-cli'));
        console.log(chalk.gray('  ─────────────────────────────────────────────'));
        console.log(chalk.white(`  Name   : ${chalk.yellow(torrentName)}`));
        console.log(chalk.white(`  Type   : ${chalk.yellow(isMultiFile ? `Multi-file (${torrent.info.files.length} files)` : 'Single-file')}`));
        console.log(chalk.white(`  Size   : ${chalk.yellow(formatBytes(totalSize))}`));
        console.log(chalk.white(`  Pieces : ${chalk.yellow(numPieces)} × ${chalk.yellow(torrent.info['piece length'] / 1024 + ' KiB')}`));
        console.log(chalk.white(`  Output : ${chalk.gray(outputDir)}`));
        console.log(chalk.gray('  ─────────────────────────────────────────────'));

        if (isMultiFile) {
            console.log(chalk.gray('  Files:'));
            torrent.info.files.forEach(f => {
                const parts = f.path.map(p => Buffer.isBuffer(p) ? p.toString('utf8') : String(p));
                console.log(chalk.gray(`    • ${parts.join('/')} (${formatBytes(f.length)})`));
            });
            console.log(chalk.gray('  ─────────────────────────────────────────────'));
        }

        console.log('');

        // ── Resume check ──────────────────────────────────────────────────
        process.stdout.write(chalk.gray('  Checking for existing progress... '));
        const { pieces, resumed } = buildResumeState(torrent, fileManager, numPieces);
        if (resumed > 0) {
            console.log(chalk.green(`resumed ${resumed}/${numPieces} pieces verified ✓`));
        } else {
            console.log(chalk.gray('none found, starting fresh'));
        }
        console.log('');

        if (pieces.isDone()) {
            console.log(chalk.bold.green('  ✓ File already complete — switching to seed mode'));
            console.log('');
            await seedOnly(torrent, fileManager, pieces, numPieces);
            return;
        }

        // ── Progress bar ──────────────────────────────────────────────────
        const bar = new cliProgress.SingleBar({
            format:
                '  {bar} {percentage}%\n' +
                '  ↓ {downSpeed}  ↑ {upSpeed}  |  {downloaded}/{total}  |  ' +
                'ETA {eta}  |  Peers ↓{dlPeers} ↑{seedPeers}  |  ' +
                '✓ {verified}  ✗ {failed}',
            barCompleteChar: '█',
            barIncompleteChar: '░',
            hideCursor: true,
            clearOnComplete: false,
            forceRedraw: true,
        }, cliProgress.Presets.shades_classic);

        const stats = new Stats();
        let verifiedCount = resumed;
        let failedCount = 0;
        let dlPeers = 0;
        let seedPeers = 0;

        bar.start(numPieces, verifiedCount, {
            downSpeed: '-- KB/s',
            upSpeed: '-- KB/s',
            downloaded: formatBytes(0),
            total: formatBytes(totalSize),
            eta: '--:--',
            dlPeers: 0,
            seedPeers: 0,
            verified: verifiedCount,
            failed: failedCount,
        });

        function updateBar() {
            bar.update(verifiedCount, {
                downSpeed: stats.downSpeed(),
                upSpeed: stats.upSpeed(),
                downloaded: formatBytes(stats.downloaded),
                total: formatBytes(totalSize),
                eta: stats.eta((numPieces - verifiedCount) * torrent.info['piece length']),
                dlPeers,
                seedPeers,
                verified: verifiedCount,
                failed: failedCount,
            });
        }

        // ── Start seeder immediately (uploads while downloading) ──────────
        const seeder = new Seeder(torrent, fileManager, pieces, CLIENT_PORT);
        seeder.onUpload = (bytes) => {
            stats.addUp(bytes);
            updateBar();
        };
        seeder.start();
        seedPeers = 0;

        // Track seed connections for UI
        const origHandleIncoming = seeder._handleIncoming.bind(seeder);
        seeder._handleIncoming = (socket) => {
            seedPeers++;
            updateBar();
            socket.on('close', () => { seedPeers = Math.max(0, seedPeers - 1); updateBar(); });
            origHandleIncoming(socket);
        };

        const hooks = {
            onVerified: (pieceIndex, bytes) => {
                verifiedCount++;
                stats.addDown(bytes);
                seeder.announceHave(pieceIndex); // tell seed peers about new piece
                updateBar();
            },
            onFailed: () => {
                failedCount++;
                updateBar();
            },
            onDlPeersChanged: (n) => {
                dlPeers = n;
                updateBar();
            }
        };

        await announceLoop(torrent, pieces, fileManager, numPieces, totalSize, stats, hooks);

        bar.stop();

        // ── Summary ───────────────────────────────────────────────────────
        console.log('');
        console.log(chalk.bold.green('  ✓ Download complete!'));
        console.log(chalk.gray(`  Elapsed  : ${stats.elapsed()}`));
        console.log(chalk.gray(`  Downloaded: ${formatBytes(stats.downloaded)}`));
        console.log(chalk.gray(`  Uploaded  : ${formatBytes(stats.uploaded)}`));
        console.log(chalk.gray(`  Verified  : ${verifiedCount} pieces`));
        console.log(chalk.gray(`  Bad pieces: ${failedCount} (re-requested)`));
        console.log('');
        console.log(chalk.bold.cyan('  Switching to seed mode... (Ctrl+C to stop)'));
        console.log('');

        // ── Keep seeding after download ────────────────────────────────────
        await seedOnly(torrent, fileManager, pieces, numPieces, seeder, stats, updateBar);

    } catch (error) {
        console.error(chalk.red('\n  Download error:'), error.message);
    }
}

// ── Seed-only mode (after download completes, or if already complete) ──────
async function seedOnly(torrent, fileManager, pieces, numPieces, existingSeeder, stats, updateBar) {
    const seeder = existingSeeder || new Seeder(torrent, fileManager, pieces, CLIENT_PORT);

    if (!existingSeeder) {
        seeder.start();
        console.log(chalk.gray('  Press Ctrl+C to stop seeding'));
        console.log('');
    }

    // Keep process alive + show upload stats every 5s
    return new Promise(() => {
        setInterval(() => {
            if (stats && updateBar) {
                updateBar();
            } else {
                const up = seeder.uploaded;
                process.stdout.write(
                    chalk.gray(`\r  Seeding... uploaded ${formatBytes(up)} to ${seeder.connections.size} peers  `)
                );
            }
        }, 5000);
    });
}

// ── Announce loop ─────────────────────────────────────────────────────────
async function announceLoop(torrent, pieces, fileManager, numPieces, totalSize, stats, hooks) {
    let attempt = 0;

    while (!pieces.isDone()) {
        attempt++;

        const peers = await new Promise(resolve =>
            getPeers(torrent, CLIENT_PORT, peers => resolve(peers))
        );

        const validPeers = peers.filter(peer =>
            peer &&
            typeof peer.port === 'number' &&
            peer.port > 0 &&
            peer.port < 65536 &&
            !INVALID_IPS.has(peer.ip)
        );

        if (validPeers.length === 0) {
            await sleep(15000);
            continue;
        }

        await runPeerBatch(validPeers, torrent, pieces, fileManager, numPieces, totalSize, hooks);

        if (!pieces.isDone()) await sleep(10000);
    }

    fileManager.close();
}

// ── Peer batch ────────────────────────────────────────────────────────────
function runPeerBatch(peers, torrent, pieces, fileManager, numPieces, totalSize, hooks) {
    return new Promise(resolve => {
        let peerIndex = 0;
        let active = 0;

        function spawnNext() {
            if (pieces.isDone()) {
                if (active === 0) resolve();
                return;
            }

            while (active < MAX_ACTIVE && peerIndex < peers.length) {
                const peer = peers[peerIndex++];
                active++;
                hooks.onDlPeersChanged(active);

                connectToPeer(peer, torrent, pieces, fileManager, numPieces, totalSize, hooks, () => {
                    active--;
                    hooks.onDlPeersChanged(active);
                    if (pieces.isDone()) {
                        if (active === 0) resolve();
                    } else {
                        spawnNext();
                        if (peerIndex >= peers.length && active === 0) resolve();
                    }
                });
            }

            if (peerIndex >= peers.length && active === 0) resolve();
        }

        spawnNext();
    });
}

// ── Connect to peer ───────────────────────────────────────────────────────
function connectToPeer(peer, torrent, pieces, fileManager, numPieces, totalSize, hooks, onClose) {
    if (!peer?.ip || !peer?.port || peer.port <= 0 || peer.port >= 65536) return onClose();
    if (INVALID_IPS.has(peer.ip)) return onClose();

    const socket = net.Socket();
    let closed = false;

    function close() {
        if (closed) return;
        closed = true;
        try { socket.destroy(); } catch (_) {}
        onClose();
    }

    socket.setTimeout(15000);
    socket.on('timeout', () => close());
    socket.on('error', () => close());
    socket.on('close', () => close());

    socket.connect(peer.port, peer.ip, () => {
        socket.write(buildHandshake(torrent));
        setupMessageHandler(socket, torrent, pieces, fileManager, numPieces, totalSize, hooks, close);
    });
}

// ── Message handler ───────────────────────────────────────────────────────
function setupMessageHandler(socket, torrent, pieces, fileManager, numPieces, totalSize, hooks, close) {
    let buffer = Buffer.alloc(0);
    let handshake = true;
    const queue = { choked: true, queue: [] };
    const pieceBuffers = {}; // accumulate blocks per piece before verifying

    socket.on('data', data => {
        buffer = Buffer.concat([buffer, data]);
        processBuffer();
    });

    function processBuffer() {
        try {
            if (buffer.length === 0) return;
            let length;
            if (handshake) {
                if (buffer.length < 1) return;
                length = buffer[0] + 49;
            } else {
                if (buffer.length < 4) return;
                length = buffer.readUInt32BE(0) + 4;
            }
            if (length <= 0 || length > 131072) {
                buffer = Buffer.alloc(0);
                return;
            }
            if (buffer.length >= length) {
                const message = buffer.subarray(0, length);
                buffer = buffer.subarray(length);
                handshake = false;
                handleMessage(message);
                processBuffer();
            }
        } catch { close(); }
    }

    function handleMessage(msg) {
        if (isHandshake(msg)) { socket.write(buildInterested()); return; }
        if (msg.length < 4) return;
        const { id, payload } = parseMessage(msg);
        switch (id) {
            case 0: handleChoke(); break;
            case 1: handleUnchoke(); break;
            case 4: handleHave(payload); break;
            case 5: handleBitfield(payload); break;
            case 7: handlePiece(payload); break;
        }
    }

    function isHandshake(msg) {
        return msg.length >= 20 &&
            msg.length === msg.readUInt8(0) + 49 &&
            msg.toString('utf8', 1, 20) === 'BitTorrent protocol';
    }

    function handleChoke()   { queue.choked = true; close(); }
    function handleUnchoke() { queue.choked = false; requestPiece(); }

    function handleHave(payload) {
        if (!payload || payload.length < 4) return;
        const pieceIndex = payload.readUInt32BE(0);
        if (pieceIndex >= 0 && pieceIndex < numPieces) {
            queue.queue.push(pieceIndex);
            if (queue.queue.length === 1) requestPiece();
        }
    }

    function handleBitfield(payload) {
        if (!payload) return;
        for (let i = 0; i < payload.length; i++) {
            for (let j = 0; j < 8; j++) {
                const pieceIndex = i * 8 + j;
                if (pieceIndex < numPieces && (payload[i] >> (7 - j)) & 1) {
                    queue.queue.push(pieceIndex);
                }
            }
        }
        requestPiece();
    }

    function handlePiece(payload) {
        if (!payload?.block) return;

        const pieceIndex = payload.index;
        const pieceLength = torrent.info['piece length'];
        const isLast = pieceIndex === numPieces - 1;
        const totalPieceLen = isLast
            ? totalSize % pieceLength || pieceLength
            : pieceLength;

        // Accumulate blocks
        if (!pieceBuffers[pieceIndex]) {
            pieceBuffers[pieceIndex] = Buffer.alloc(totalPieceLen);
        }
        payload.block.copy(pieceBuffers[pieceIndex], payload.begin);

        // Check if piece is fully assembled
        const isLastBlock = payload.begin + payload.block.length >= totalPieceLen;
        if (!isLastBlock) return;

        const pieceBuf = pieceBuffers[pieceIndex];
        delete pieceBuffers[pieceIndex];

        // SHA-1 verify
        if (verifyPiece(torrent, pieceIndex, pieceBuf)) {
            try {
                fileManager.writePiece(pieceIndex, pieceBuf); // multi-file aware write
                pieces.addReceived(pieceIndex);
                hooks.onVerified(pieceIndex, pieceBuf.length);
                queue.queue.shift();
                requestPiece();
                if (pieces.isDone()) close();
            } catch (err) {
                close();
            }
        } else {
            // Bad piece — un-mark and re-queue
            pieces.requested.delete(pieceIndex);
            queue.queue.push(pieceIndex);
            hooks.onFailed();
            requestPiece();
        }
    }

    function requestPiece() {
        if (queue.choked) return;
        while (queue.queue.length) {
            const pieceIndex = queue.queue[0];
            if (pieces.needed(pieceIndex)) {
                const pieceLength = torrent.info['piece length'];
                const isLast = pieceIndex === numPieces - 1;
                const totalPieceLen = isLast
                    ? totalSize % pieceLength || pieceLength
                    : pieceLength;

                for (let begin = 0; begin < totalPieceLen; begin += BLOCK_SIZE) {
                    const blockLen = Math.min(BLOCK_SIZE, totalPieceLen - begin);
                    try {
                        socket.write(buildRequest({ index: pieceIndex, begin, length: blockLen }));
                    } catch (_) { return; }
                }
                pieces.addRequested(pieceIndex);
                queue.queue.shift();
                break;
            }
            queue.queue.shift();
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────
function formatBytes(bytes) {
    if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(2)} GB`;
    if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(2)} MB`;
    if (bytes >= 1e3) return `${(bytes / 1e3).toFixed(2)} KB`;
    return `${bytes} B`;
}

function sleep(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
}