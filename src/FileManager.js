/**
 * FileManager.js
 *
 * Handles multi-file torrent layout. In BitTorrent, pieces span file
 * boundaries — a single piece can contain the tail of one file and the
 * head of the next. This module builds a flat file map and translates
 * any piece+offset into the correct file descriptor(s) and byte positions.
 *
 * Single-file torrents are treated as a degenerate case of multi-file.
 */

import fs from 'fs';
import path from 'path';

export default class FileManager {
    /**
     * @param {object} torrent   - decoded torrent object
     * @param {string} outputDir - directory to write files into
     */
    constructor(torrent, outputDir) {
        this.torrent = torrent;
        this.outputDir = outputDir;
        this.pieceLength = torrent.info['piece length'];

        // Build a flat list of { filePath, length, offset } where offset
        // is the byte position of the file's start within the torrent's
        // logical byte stream.
        this.files = this._buildFileList();
        this.totalSize = this.files.reduce((s, f) => s + f.length, 0);

        // Open all file descriptors
        this.fds = this._openFiles();
    }

    // ── Public API ─────────────────────────────────────────────────────────

    /**
     * Write a verified piece to the correct file(s).
     * A piece may span multiple files — this handles all cases.
     */
    writePiece(pieceIndex, pieceBuf) {
        const pieceStart = pieceIndex * this.pieceLength;

        let bufOffset = 0;
        while (bufOffset < pieceBuf.length) {
            const globalOffset = pieceStart + bufOffset;
            const file = this._fileAt(globalOffset);
            if (!file) break;

            const fileOffset = globalOffset - file.offset;
            const bytesRemaining = pieceBuf.length - bufOffset;
            const spaceInFile = file.length - fileOffset;
            const bytesToWrite = Math.min(bytesRemaining, spaceInFile);

            fs.writeSync(
                this.fds[file.filePath],
                pieceBuf,
                bufOffset,
                bytesToWrite,
                fileOffset
            );

            bufOffset += bytesToWrite;
        }
    }

    /**
     * Read a piece from disk (for seeding — serving pieces to peers).
     * Returns a Buffer of the full piece, or null if any read fails.
     */
    readPiece(pieceIndex, pieceLength) {
        const pieceStart = pieceIndex * this.pieceLength;
        const result = Buffer.alloc(pieceLength);

        let bufOffset = 0;
        while (bufOffset < pieceLength) {
            const globalOffset = pieceStart + bufOffset;
            const file = this._fileAt(globalOffset);
            if (!file) break;

            const fileOffset = globalOffset - file.offset;
            const bytesRemaining = pieceLength - bufOffset;
            const spaceInFile = file.length - fileOffset;
            const bytesToRead = Math.min(bytesRemaining, spaceInFile);

            try {
                const bytesRead = fs.readSync(
                    this.fds[file.filePath],
                    result,
                    bufOffset,
                    bytesToRead,
                    fileOffset
                );
                if (bytesRead < bytesToRead) return null;
            } catch {
                return null;
            }

            bufOffset += bytesToRead;
        }

        return result;
    }

    /**
     * Read a full piece for resume verification.
     */
    readPieceForVerify(pieceIndex, numPieces) {
        const isLast = pieceIndex === numPieces - 1;
        const thisPieceLen = isLast
            ? this.totalSize % this.pieceLength || this.pieceLength
            : this.pieceLength;
        return this.readPiece(pieceIndex, thisPieceLen);
    }

    /**
     * Build a bitfield Buffer representing which pieces are available.
     * Used when seeding to advertise what we have to connecting peers.
     */
    buildBitfield(receivedSet, numPieces) {
        const numBytes = Math.ceil(numPieces / 8);
        const bitfield = Buffer.alloc(numBytes, 0);
        for (const pieceIndex of receivedSet) {
            const byteIndex = Math.floor(pieceIndex / 8);
            const bitIndex = 7 - (pieceIndex % 8);
            bitfield[byteIndex] |= (1 << bitIndex);
        }
        return bitfield;
    }

    /** Close all open file descriptors */
    close() {
        for (const fd of Object.values(this.fds)) {
            try { fs.closeSync(fd); } catch (_) {}
        }
    }

    // ── Private ────────────────────────────────────────────────────────────

    _buildFileList() {
        const name = this.torrent.info.name.toString('utf8');

        if (this.torrent.info.files) {
            // Multi-file torrent
            let offset = 0;
            return this.torrent.info.files.map(file => {
                const parts = file.path.map(p =>
                    Buffer.isBuffer(p) ? p.toString('utf8') : String(p)
                );
                const filePath = path.join(this.outputDir, name, ...parts);
                const entry = { filePath, length: file.length, offset };
                offset += file.length;
                return entry;
            });
        } else {
            // Single-file torrent
            return [{
                filePath: path.join(this.outputDir, name),
                length: this.torrent.info.length,
                offset: 0
            }];
        }
    }

    _openFiles() {
        const fds = {};
        for (const file of this.files) {
            // Create parent directories if needed
            fs.mkdirSync(path.dirname(file.filePath), { recursive: true });

            if (fs.existsSync(file.filePath)) {
                fds[file.filePath] = fs.openSync(file.filePath, 'r+');
            } else {
                fds[file.filePath] = fs.openSync(file.filePath, 'w');
                fs.ftruncateSync(fds[file.filePath], file.length);
            }
        }
        return fds;
    }

    /** Find the file that contains the given global byte offset */
    _fileAt(globalOffset) {
        for (const file of this.files) {
            if (globalOffset >= file.offset && globalOffset < file.offset + file.length) {
                return file;
            }
        }
        return null;
    }
}