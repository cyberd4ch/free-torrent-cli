/**
 * Seeder.js
 *
 * Listens for incoming peer connections and serves pieces to them.
 * Implements the leecher side of the tit-for-tat protocol:
 *   - Sends our bitfield on connect
 *   - Responds to interested/not-interested
 *   - Unchokes peers and serves piece requests
 *   - Sends have messages when new pieces are received
 */

import net from 'net';
import { buildHandshake, buildBitField, buildUnchoke, buildHave, buildPiece, buildChoke, parseMessage } from './Message.js';
import { getInfoHash } from './TorrentParser.js';

const MAX_SEED_CONNECTIONS = 20;
const UPLOAD_SLOT_COUNT = 4; // max simultaneous uploaders (tit-for-tat)

export default class Seeder {
    /**
     * @param {object}      torrent     - decoded torrent
     * @param {FileManager} fileManager - for reading pieces off disk
     * @param {Pieces}      pieces      - shared piece state
     * @param {number}      port        - port to listen on
     */
    constructor(torrent, fileManager, pieces, port = 6881) {
        this.torrent = torrent;
        this.fileManager = fileManager;
        this.pieces = pieces;
        this.port = port;
        this.numPieces = Math.ceil(torrent.info.pieces.length / 20);
        this.server = null;
        this.connections = new Map(); // socket → peerState
        this.uploadSlots = 0;
        this.uploaded = 0;
        this.onUpload = null; // hook for UI updates
    }

    // ── Start / Stop ───────────────────────────────────────────────────────

    start() {
        this.server = net.createServer(socket => {
            if (this.connections.size >= MAX_SEED_CONNECTIONS) {
                socket.destroy();
                return;
            }
            this._handleIncoming(socket);
        });

        this.server.on('error', err => {
            if (err.code === 'EADDRINUSE') {
                // Port in use — try next port
                this.port++;
                this.server.listen(this.port);
            }
        });

        this.server.listen(this.port, () => {
            console.log(`  Seeding on port ${this.port}`);
        });
    }

    stop() {
        for (const socket of this.connections.keys()) {
            try { socket.destroy(); } catch (_) {}
        }
        this.connections.clear();
        if (this.server) {
            this.server.close();
            this.server = null;
        }
    }

    /**
     * Notify all connected peers that we now have a new piece.
     * Called by download.js whenever a piece is verified.
     */
    announceHave(pieceIndex) {
        const msg = buildHave(pieceIndex);
        for (const [socket, state] of this.connections) {
            if (state.handshakeDone && !state.closed) {
                try { socket.write(msg); } catch (_) {}
            }
        }
    }

    // ── Incoming connection handler ────────────────────────────────────────

    _handleIncoming(socket) {
        const state = {
            handshakeDone: false,
            peerInterested: false,
            weChoking: true,
            buffer: Buffer.alloc(0),
            closed: false,
        };

        this.connections.set(socket, state);

        socket.setTimeout(30000);
        socket.on('timeout', () => this._closeConn(socket));
        socket.on('error', () => this._closeConn(socket));
        socket.on('close', () => this._closeConn(socket));

        socket.on('data', data => {
            state.buffer = Buffer.concat([state.buffer, data]);
            this._processBuffer(socket, state);
        });
    }

    _closeConn(socket) {
        const state = this.connections.get(socket);
        if (!state || state.closed) return;
        state.closed = true;
        if (!state.weChoking) this.uploadSlots--;
        this.connections.delete(socket);
        try { socket.destroy(); } catch (_) {}
    }

    // ── Message parsing ────────────────────────────────────────────────────

    _processBuffer(socket, state) {
        try {
            while (true) {
                if (state.buffer.length === 0) break;

                let length;
                if (!state.handshakeDone) {
                    if (state.buffer.length < 1) break;
                    length = state.buffer[0] + 49;
                } else {
                    if (state.buffer.length < 4) break;
                    length = state.buffer.readUInt32BE(0) + 4;
                }

                if (length <= 0 || length > 131072) {
                    state.buffer = Buffer.alloc(0);
                    break;
                }

                if (state.buffer.length < length) break;

                const msg = state.buffer.subarray(0, length);
                state.buffer = state.buffer.subarray(length);

                if (!state.handshakeDone) {
                    this._handleHandshake(socket, state, msg);
                } else {
                    this._handleMessage(socket, state, msg);
                }
            }
        } catch {
            this._closeConn(socket);
        }
    }

    _handleHandshake(socket, state, msg) {
        // Validate it's a BitTorrent handshake
        if (
            msg.length < 68 ||
            msg.toString('utf8', 1, 20) !== 'BitTorrent protocol'
        ) {
            this._closeConn(socket);
            return;
        }

        // Validate info hash matches our torrent
        const theirHash = msg.slice(28, 48);
        const ourHash = getInfoHash(this.torrent);
        if (!theirHash.equals(ourHash)) {
            this._closeConn(socket);
            return;
        }

        state.handshakeDone = true;

        // Send our handshake back
        socket.write(buildHandshake(this.torrent));

        // Send our bitfield so peer knows what we have
        const bitfield = this.fileManager.buildBitfield(
            this.pieces.received,
            this.numPieces
        );
        socket.write(buildBitField(bitfield));
    }

    _handleMessage(socket, state, msg) {
        if (msg.length < 4) return;

        // Keep-alive
        if (msg.readUInt32BE(0) === 0) return;

        const { id, payload } = parseMessage(msg);

        switch (id) {
            case 2: // interested
                state.peerInterested = true;
                this._tryUnchoke(socket, state);
                break;

            case 3: // not interested
                state.peerInterested = false;
                this._chokePeer(socket, state);
                break;

            case 6: // request
                this._handleRequest(socket, state, payload);
                break;

            case 8: // cancel — nothing to do (we send whole blocks synchronously)
                break;
        }
    }

    // ── Upload logic ───────────────────────────────────────────────────────

    _tryUnchoke(socket, state) {
        if (!state.weChoking) return; // already unchoked
        if (this.uploadSlots >= UPLOAD_SLOT_COUNT) return; // slots full

        this.uploadSlots++;
        state.weChoking = false;
        try { socket.write(buildUnchoke()); } catch (_) {}
    }

    _chokePeer(socket, state) {
        if (state.weChoking) return;
        state.weChoking = true;
        this.uploadSlots--;
        try { socket.write(buildChoke()); } catch (_) {}
    }

    _handleRequest(socket, state, payload) {
        if (!payload) return;
        if (state.weChoking) return; // peer is choked, ignore request

        const { index, begin, length } = payload;

        // Validate request
        if (
            typeof index !== 'number' ||
            typeof begin !== 'number' ||
            !length ||
            !this.pieces.received.has(index)
        ) return;

        const blockLength = Buffer.isBuffer(length) ? length.readUInt32BE(0) : length;

        // Guard against oversized requests (max 32 KiB)
        if (blockLength > 32768) return;

        const pieceLength = this.torrent.info['piece length'];
        const isLast = index === this.numPieces - 1;
        const thisPieceLen = isLast
            ? this.fileManager.totalSize % pieceLength || pieceLength
            : pieceLength;

        const pieceBuf = this.fileManager.readPiece(index, thisPieceLen);
        if (!pieceBuf) return;

        const block = pieceBuf.slice(begin, begin + blockLength);

        try {
            socket.write(buildPiece({ index, begin, block }));
            this.uploaded += block.length;
            if (this.onUpload) this.onUpload(block.length);
        } catch (_) {}
    }
}