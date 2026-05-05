import { Buffer } from 'buffer';
import { getInfoHash } from './TorrentParser.js';
import { generatePeerId } from './util.js';

export function buildHandshake(torrent) {
    const buffer = Buffer.alloc(68);

    // pstrlen
    buffer.writeUInt8(19, 0);

    // pstr
    buffer.write('BitTorrent protocol', 1);

    // reserved bytes
    buffer.writeUInt32BE(0, 20);
    buffer.writeUInt32BE(0, 24);

    // info hash
    getInfoHash(torrent).copy(buffer, 28);

    // peer id
    generatePeerId().copy(buffer, 48);
    return buffer;
}

export function buildKeepAlive() {
    return Buffer.alloc(4);
}

export function buildChoke() {
    const buffer = Buffer.alloc(5);
    buffer.writeUInt32BE(1, 0);
    buffer.writeUInt8(0, 4);
    return buffer;
}

export function buildUnchoke() {
    const buffer = Buffer.alloc(5);
    buffer.writeUInt32BE(1, 0);
    buffer.writeUInt8(2, 4);
    return buffer;
}

export function buildInterested() {
    const buffer = Buffer.alloc(5);
    buffer.writeUInt32BE(1, 0);
    buffer.writeUInt8(3, 4);
    return buffer;
}

export function buildHave(payload) {
    const buffer = Buffer.alloc(9);
    buffer.writeUInt32BE(5, 0);
    buffer.writeUInt8(4, 4);
    buffer.writeUInt32BE(payload, 5);
    return buffer;
}

export function buildBitField(bitfield) {
    // Fixed: use bitfield.length (not undefined payload.length)
    const buffer = Buffer.alloc(bitfield.length + 6);
    buffer.writeUInt32BE(bitfield.length + 1, 0);
    buffer.writeUInt8(5, 4);
    bitfield.copy(buffer, 5);
    return buffer;
}

export function buildRequest(payload) {
    // Fixed: buffer must be 17 bytes (4 len + 1 id + 4 index + 4 begin + 4 length)
    const buffer = Buffer.alloc(17);
    buffer.writeUInt32BE(13, 0);
    buffer.writeUInt8(6, 4);
    buffer.writeUInt32BE(payload.index, 5);
    buffer.writeUInt32BE(payload.begin, 9);
    buffer.writeUInt32BE(payload.length, 13);
    return buffer;
}

export function buildPiece(payload) {
    const buffer = Buffer.alloc(payload.block.length + 13);
    buffer.writeUInt32BE(payload.block.length + 9, 0);
    buffer.writeUInt8(7, 4);
    buffer.writeUInt32BE(payload.index, 5);
    buffer.writeUInt32BE(payload.begin, 9);
    payload.block.copy(buffer, 13);
    return buffer;
}

export function buildCancel(payload) {
    const buffer = Buffer.alloc(17);
    buffer.writeUInt32BE(13, 0);
    buffer.writeUInt8(8, 4);
    buffer.writeUInt32BE(payload.index, 5);
    buffer.writeUInt32BE(payload.begin, 9);
    buffer.writeUInt32BE(payload.length, 13);
    return buffer;
}

export function buildPort(payload) {
    const buffer = Buffer.alloc(7);
    buffer.writeUInt32BE(3, 0);
    buffer.writeUInt8(9, 4);
    buffer.writeUInt16BE(payload, 5);
    return buffer;
}

export function parseMessage(message) {
    const length = message.readInt32BE(0);
    const id = length > 0 ? message.readInt8(4) : null;

    // Fixed: use `let` so payload can be reassigned to structured object
    let payload = length > 0 ? message.subarray(5) : null;

    if (id === 6 || id === 7 || id === 8) {
        const rest = payload.slice(8);
        payload = {
            index: payload.readInt32BE(0),
            begin: payload.readInt32BE(4),
            [id === 7 ? 'block' : 'length']: rest
        };
    }

    return { length, id, payload };
}