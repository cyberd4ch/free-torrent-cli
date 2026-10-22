/**
 * src/MetadataExchange.js — Fetch torrent metadata from peers (BEP 9/10).
 *
 * With only a magnet link we don't know piece sizes, piece hashes or the
 * file layout. We learn those by talking `ut_metadata` to peers:
 *
 *   1. Handshake with the extension-protocol bit set in reserved bytes
 *   2. Exchange `extended handshake` dicts (ext id 0) to agree on ids
 *   3. Request metadata `pieces` and assemble the full info dict
 *   4. Verify SHA-1(bencode(info dict)) == info hash — the only trust
 *      anchor we need
 */

import net from 'net';
import crypto from 'crypto';
import bencode from 'bencode';
import { Buffer } from 'buffer';
import { generatePeerId } from './util.js';

// bencode v4 returns a raw Uint8Array — normalise everything to Buffer.
const enc = (...args) => Buffer.from(bencode.encode(...args));

const MSG_HANDSHAKE_ID = 0;
const MSG_TYPE_REQUEST = 0;
const MSG_TYPE_DATA = 1;
const MSG_TYPE_REJECT = 2;
const METADATA_BLOCK = 16384; // 16 KiB, fixed by BEP 9

// ── minimal bencode prefix decoder ─────────────────────────────────────────
// The ut_metadata data response is `bencode(dict) + raw bytes` in one
// message, so on top of bencode.decode() we need to know where the dict
// ends inside the raw payload.
export function bencodePrefixEnd(buf, offset = 0) {
    if (offset >= buf.length) throw new Error('empty bencode value');
    const start = buf[offset];

    if (start === 0x64) { // 'd' dict
        let pos = offset + 1;
        while (buf[pos] !== 0x65) { // 'e'
            pos = bencodePrefixEnd(buf, pos);        // key
            pos = bencodePrefixEnd(buf, pos);        // value
        }
        return pos + 1;
    }
    if (start === 0x6c) { // 'l' list
        let pos = offset + 1;
        while (buf[pos] !== 0x65) pos = bencodePrefixEnd(buf, pos);
        return pos + 1;
    }
    if (start === 0x69) { // 'i' integer
        const end = buf.indexOf(0x65, offset);
        if (end === -1) throw new Error('unterminated integer');
        return end + 1;
    }
    // byte string: <digits>:<bytes>
    const colon = buf.indexOf(0x3a, offset);
    if (colon === -1) throw new Error('unterminated string');
    const len = parseInt(buf.toString('utf8', offset, colon), 10);
    return colon + 1 + len;
}

// ── building blocks ────────────────────────────────────────────────────────
/** Handshake with the BEP 10 extension bit set in reserved byte 5. */
export function buildExtendedHandshakeBytes(infoHash, peerId = generatePeerId()) {
    const buffer = Buffer.alloc(68);
    buffer.writeUInt8(19, 0);
    buffer.write('BitTorrent protocol', 1);
    buffer.writeUInt32BE(0, 20);
    buffer.writeUInt32BE(0, 24);
    buffer[25] = 0x10;                     // extension protocol bit
    infoHash.copy(buffer, 28);
    peerId.copy(buffer, 48);
    return buffer;
}

/** Generic extended message: <len><0x14><extended id><payload>. */
export function buildExtendedMessage(extendedId, payload) {
    payload = Buffer.from(payload); // tolerate bencode v4's Uint8Array
    // Frame: 4-byte length prefix, id byte, extended-id byte, payload.
    const buffer = Buffer.alloc(6 + payload.length);
    buffer.writeUInt32BE(2 + payload.length, 0);
    buffer.writeUInt8(20, 4);              // extended message id
    buffer.writeUInt8(extendedId, 5);
    payload.copy(buffer, 6);
    return buffer;
}

export function buildExtendedHandshakeDict(utMetadataId, ourPort = 6881) {
    return buildExtendedMessage(MSG_HANDSHAKE_ID, enc({
        m: { ut_metadata: utMetadataId },
        p: ourPort,
        v: 'free-torrent-cli 2.0',
        e: 0,
    }));
}

export function buildMetadataRequest(utMetadataId, pieceIndex) {
    return buildExtendedMessage(utMetadataId, enc({
        msg_type: MSG_TYPE_REQUEST,
        piece: pieceIndex,
    }));
}

export function buildMetadataResponse(utMetadataId, pieceIndex, totalSize, data) {
    const dict = enc({ msg_type: MSG_TYPE_DATA, piece: pieceIndex, total_size: totalSize });
    return buildExtendedMessage(utMetadataId, Buffer.concat([dict, data]));
}

export function buildMetadataReject(utMetadataId, pieceIndex) {
    return buildExtendedMessage(utMetadataId, enc({ msg_type: MSG_TYPE_REJECT, piece: pieceIndex }));
}

/**
 * Parse one post-handshake framed message.
 * Returns { id, extendedId, dict, data } — extendedId/dict/data are null
 * for non-extended messages.
 */
export function parseExtendedMessage(msg) {
    const length = msg.readInt32BE(0);
    const id = length > 0 ? msg.readUInt8(4) : null;
    const body = length > 0 ? msg.subarray(5) : Buffer.alloc(0);

    if (id !== 20) return { id, extendedId: null, dict: null, data: null };

    const extendedId = body.length > 0 ? body.readUInt8(0) : null;
    const payload = extendedId !== null ? body.subarray(1) : Buffer.alloc(0);

    if (!payload.length || payload[0] !== 0x64) {
        return { id, extendedId, dict: null, data: payload.length ? payload : null };
    }

    const dictEnd = bencodePrefixEnd(payload, 0);
    const dictDecode = bencode.decode(payload.subarray(0, dictEnd));
    const dict = {};
    for (const [k, v] of Object.entries(dictDecode)) {
        dict[k] = v instanceof Uint8Array ? Buffer.from(v) : v;
    }
    const data = payload.length > dictEnd ? payload.subarray(dictEnd) : null;
    return { id, extendedId, dict, data };
}

// ── the fetcher ────────────────────────────────────────────────────────────
/**
 * Ask peers for metadata pieces until one connection delivers everything.
 *
 * hosts: [{ip, port}] — sourced from DHT / magnet trackers.
 * Returns the decoded `info` dict (Buffers preserved).
 */
export async function fetchMetadata({
    infoHash,
    hosts = [],
    timeoutMs = 30000,
    onProgress = () => {},
}) {
    if (!infoHash || infoHash.length !== 20) {
        throw new Error('fetchMetadata needs a 20-byte infoHash');
    }
    if (!hosts.length) throw new Error('no peers to fetch metadata from');

    return new Promise((resolve, reject) => {
        let settled = false;
        const finish = (fn, value) => {
            if (settled) return;
            settled = true;
            clearTimeout(outerTimer);
            fn(value);
        };
        const outerTimer = setTimeout(
            () => finish(reject, new Error('metadata fetch timed out')),
            timeoutMs
        );

        const candidates = hosts.filter(Boolean).slice(0, 8);
        if (!candidates.length) {
            finish(reject, new Error('no peers to fetch metadata from'));
            return;
        }
        let remaining = candidates.length;

        for (const host of candidates) {
            connectMetadataPeer(host, infoHash, {
                onInfo(infoRaw) {
                    try {
                        const info = normaliseInfo(infoRaw);
                        const encoded = enc(info);
                        if (crypto.createHash('sha1').update(encoded).digest().equals(infoHash)) {
                            finish(resolve, info);
                        } else {
                            finishPeer(); // wrong metadata — spend the connection
                        }
                    } catch (_) { finishPeer(); }
                },
                onProgress,
                onPeerDone: finishPeer,
            });

            let peerDone = false;
            function finishPeer() {
                if (peerDone) return;
                peerDone = true;
                if (--remaining <= 0) {
                    finish(reject, new Error('no peer provided the metadata (was anyone actually seeding?)'));
                }
            }
        }
    });
}

/**
 * One TCP connection: handshake → extended handshake → request all pieces.
 * `onInfo(infoDict)` fires when every requested piece arrived (or on our
 * side is otherwise done); `onPeerDone()` always fires afterwards exactly
 * once so callers can track connection budget.
 */
function connectMetadataPeer({ ip, port }, infoHash, ctx) {
    const OUR_UT_ID = 1; // local ut_metadata id we advertise per connection
    const socket = new net.Socket();
    let buffer = Buffer.alloc(0);
    let phase = 'handshake';
    let theirUtId = null;
    let numPieces = null;
    const pieces = {};   // index → Buffer
    let doneFired = false;

    function onDone() {
        if (doneFired) return;
        doneFired = true;
        try { socket.destroy(); } catch (_) {}
        ctx.onPeerDone();
    }

    socket.setTimeout(15000);
    socket.on('timeout', onDone);
    socket.on('error', onDone);
    socket.on('close', onDone);

    socket.connect(port, ip, () => {
        socket.write(buildExtendedHandshakeBytes(infoHash));
    });

    socket.on('data', (data) => {
        buffer = Buffer.concat([buffer, data]);
        drain();
    });

    function drain() {
        try {
            if (phase === 'handshake') {
                if (buffer.length < 68) return;
                if (!(buffer[25] & 0x10)) { onDone(); return; }  // no BEP10, no metadata
                phase = 'ehandshake';
                socket.write(buildExtendedHandshakeDict(OUR_UT_ID));
                buffer = buffer.subarray(68);
                // fall through — the peer may have batched its extended
                // handshake into the same TCP segment
            }

            if (buffer.length < 4) return;
            const framedLen = buffer.readUInt32BE(0) + 4;
            if (framedLen <= 4 || framedLen > MAX_FRAME) { buffer = Buffer.alloc(0); onDone(); return; }
            if (buffer.length < framedLen) return;

            const msg = buffer.subarray(0, framedLen);
            buffer = buffer.subarray(framedLen);

            if (phase === 'ehandshake') {
                const parsed = parseExtendedMessage(msg);
                const m = parsed.dict?.m;
                theirUtId = m?.ut_metadata;
                const metaSize = parsed.dict?.metadata_size;
                if (!m?.ut_metadata || !metaSize) { onDone(); return; }
                numPieces = Math.ceil(metaSize / METADATA_BLOCK);
                phase = 'fetching';
                for (let i = 0; i < numPieces; i++) {
                    socket.write(buildMetadataRequest(theirUtId, i));
                }
                drain();
                return;
            }

            if (phase === 'fetching') {
                const parsed = parseExtendedMessage(msg);
                if (parsed.extendedId !== theirUtId || !parsed.dict || dictMsgType(parsed.dict) !== MSG_TYPE_DATA || !parsed.data) return;

                pieces[parsed.dict.piece] = Buffer.from(parsed.data);
                ctx.onProgress(Object.keys(pieces).length);

                const have = Object.keys(pieces).length;
                if (have === numPieces) {
                    const total = Buffer.concat(
                        Array.from({ length: numPieces }, (_, i) => pieces[i])
                    );
                    const info = bencode.decode(total);
                    ctx.onInfo(info);
                }
                drain();
            }
        } catch (_) {
            onDone();
        }
    }
}

function dictMsgType(dict) {
    return dict.msg_type ?? dict['msg_type'];
}

// Recursively convert decoded values to Buffers — downstream code
// (FileManager, tracker, download) assumes Buffers throughout.
function normaliseInfo(val) {
    if (val instanceof Uint8Array || val instanceof Buffer) return Buffer.from(val);
    if (Array.isArray(val)) return val.map(normaliseInfo);
    if (val !== null && typeof val === 'object') {
        return Object.fromEntries(
            Object.entries(val).map(([k, v]) => [k, normaliseInfo(v)])
        );
    }
    return val;
}

const MAX_FRAME = 1 << 22;
