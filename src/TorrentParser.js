import { readFile } from 'fs/promises';
import bencode from 'bencode';
import crypto from 'crypto';

// Recursively convert all Uint8Array values in the decoded torrent to Buffers
function normalise(val) {
    if (val instanceof Uint8Array) return Buffer.from(val);
    if (Array.isArray(val)) return val.map(normalise);
    if (val !== null && typeof val === 'object') {
        return Object.fromEntries(
            Object.entries(val).map(([k, v]) => [k, normalise(v)])
        );
    }
    return val;
}

export async function open(filePath) {
    const data = await readFile(filePath);
    return normalise(bencode.decode(data));
}

export function size(torrent) {
    const total = BigInt(
        torrent.info.files
            ? torrent.info.files.reduce((sum, file) => sum + file.length, 0)
            : torrent.info.length
    );
    const buf = Buffer.alloc(8);
    buf.writeBigUInt64BE(total, 0);
    return buf;
}

export function getInfoHash(torrent) {
    const info = bencode.encode(torrent.info);
    return crypto.createHash('sha1').update(info).digest();
}

export default { open, size, getInfoHash };
