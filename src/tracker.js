import URLParse from 'url-parse';
import dgram from 'dgram';
import http from 'http';
import https from 'https';
import { Buffer } from 'buffer';
import crypto from 'crypto';
import { getInfoHash, size } from './TorrentParser.js';
import { generatePeerId } from './util.js';

// ── URL decoder ────────────────────────────────────────────────────────────
function decodeUrl(val) {
    if (val instanceof Uint8Array || Buffer.isBuffer(val)) return Buffer.from(val).toString('utf8');
    return String(val);
}

// ── Collect all tracker URLs from torrent ──────────────────────────────────
function getTrackerUrls(torrent) {
    const urls = [];
    if (torrent.announce) urls.push(decodeUrl(torrent.announce));
    if (torrent['announce-list']) {
        torrent['announce-list'].forEach(tier =>
            tier.forEach(url => urls.push(decodeUrl(url)))
        );
    }
    return [...new Set(urls)];
}

// ── Main entry point ───────────────────────────────────────────────────────
export function getPeers(torrent, clientPort, callback) {
    const allUrls = getTrackerUrls(torrent);

    const udpUrls = allUrls.filter(u => u.startsWith('udp://'));
    const httpUrls = allUrls.filter(u => u.startsWith('http://') || u.startsWith('https://'));

    // Try UDP first, then HTTP
    const orderedUrls = [...udpUrls, ...httpUrls];

    console.log(`Found ${udpUrls.length} UDP + ${httpUrls.length} HTTP tracker(s) to try...`);

    let tried = 0;

    function tryNext() {
        if (tried >= orderedUrls.length) {
            console.error('All trackers failed, no peers found');
            return callback([]);
        }

        const trackerUrl = orderedUrls[tried++];
        console.log(`Trying tracker ${tried}/${orderedUrls.length}: ${trackerUrl}`);

        if (trackerUrl.startsWith('udp://')) {
            tryUdpTracker(trackerUrl, torrent, clientPort, (peers) => {
                if (peers.length > 0) {
                    console.log(`Got ${peers.length} peers!`);
                    callback(peers);
                } else {
                    tryNext();
                }
            });
        } else {
            tryHttpTracker(trackerUrl, torrent, clientPort, (peers) => {
                if (peers.length > 0) {
                    console.log(`Got ${peers.length} peers!`);
                    callback(peers);
                } else {
                    tryNext();
                }
            });
        }
    }

    tryNext();
}

// ── UDP tracker ────────────────────────────────────────────────────────────
function tryUdpTracker(trackerUrl, torrent, clientPort, callback) {
    const socket = dgram.createSocket('udp4');

    const timeout = setTimeout(() => {
        try { socket.close(); } catch (_) {}
        console.warn('UDP tracker timed out');
        callback([]);
    }, 5000);

    udpSend(socket, buildConnReq(), trackerUrl);

    socket.on('message', response => {
        if (respType(response) === 'connect') {
            const connResp = parseConnResp(response);
            const announceReq = buildAnnounceReq(connResp.connectionId, torrent, clientPort);
            udpSend(socket, announceReq, trackerUrl);
        } else if (respType(response) === 'announce') {
            clearTimeout(timeout);
            const announceResp = parseAnnounceResp(response);
            try { socket.close(); } catch (_) {}
            callback(announceResp.peers);
        }
    });

    socket.on('error', err => {
        clearTimeout(timeout);
        try { socket.close(); } catch (_) {}
        console.warn(`UDP tracker error: ${err.message}`);
        callback([]);
    });
}

// ── HTTP tracker ───────────────────────────────────────────────────────────
function tryHttpTracker(trackerUrl, torrent, clientPort, callback) {
    // Build the announce URL with required query params
    const infoHash = getInfoHash(torrent);
    const peerId = generatePeerId();
    const left = size(torrent).readBigUInt64BE(0).toString();

    // Encode binary buffers as url-encoded byte strings
    function encodeBinary(buf) {
        let encoded = '';
        for (const byte of buf) {
            if (
                (byte >= 0x41 && byte <= 0x5A) || // A-Z
                (byte >= 0x61 && byte <= 0x7A) || // a-z
                (byte >= 0x30 && byte <= 0x39) || // 0-9
                byte === 0x2D || byte === 0x5F ||  // - _
                byte === 0x2E || byte === 0x7E     // . ~
            ) {
                encoded += String.fromCharCode(byte);
            } else {
                encoded += '%' + byte.toString(16).padStart(2, '0').toUpperCase();
            }
        }
        return encoded;
    }

    const params = new URLSearchParams({
        info_hash: encodeBinary(infoHash),
        peer_id: encodeBinary(peerId),
        port: clientPort,
        uploaded: '0',
        downloaded: '0',
        left,
        compact: '1',
        event: 'started'
    });

    // URLSearchParams will re-encode our binary strings — build raw URL manually
    const baseUrl = trackerUrl.includes('?')
        ? `${trackerUrl}&`
        : `${trackerUrl}?`;

    const queryString = [
        `info_hash=${encodeBinary(infoHash)}`,
        `peer_id=${encodeBinary(peerId)}`,
        `port=${clientPort}`,
        `uploaded=0`,
        `downloaded=0`,
        `left=${left}`,
        `compact=1`,
        `event=started`
    ].join('&');

    const fullUrl = baseUrl + queryString;
    const lib = fullUrl.startsWith('https') ? https : http;

    const req = lib.get(fullUrl, res => {
        const chunks = [];
        res.on('data', chunk => chunks.push(chunk));
        res.on('end', () => {
            try {
                const body = Buffer.concat(chunks);
                const peers = parseHttpTrackerResponse(body);
                callback(peers);
            } catch (err) {
                console.warn(`HTTP tracker parse error: ${err.message}`);
                callback([]);
            }
        });
    });

    req.setTimeout(8000, () => {
        req.destroy();
        console.warn('HTTP tracker timed out');
        callback([]);
    });

    req.on('error', err => {
        console.warn(`HTTP tracker error: ${err.message}`);
        callback([]);
    });
}

// ── Parse HTTP tracker bencoded response ───────────────────────────────────
function parseHttpTrackerResponse(body) {
    // Minimal bencode decoder for tracker responses
    function decode(buf, offset = 0) {
        const ch = String.fromCharCode(buf[offset]);

        if (ch === 'd') {
            // dictionary
            const dict = {};
            offset++;
            while (buf[offset] !== 0x65) { // 'e'
                const [key, o1] = decode(buf, offset);
                const [val, o2] = decode(buf, o1);
                dict[key.toString()] = val;
                offset = o2;
            }
            return [dict, offset + 1];
        }

        if (ch === 'l') {
            // list
            const list = [];
            offset++;
            while (buf[offset] !== 0x65) { // 'e'
                const [val, o] = decode(buf, offset);
                list.push(val);
                offset = o;
            }
            return [list, offset + 1];
        }

        if (ch === 'i') {
            // integer
            const end = buf.indexOf(0x65, offset); // 'e'
            const num = parseInt(buf.slice(offset + 1, end).toString(), 10);
            return [num, end + 1];
        }

        // byte string
        const colon = buf.indexOf(0x3a, offset); // ':'
        const len = parseInt(buf.slice(offset, colon).toString(), 10);
        const start = colon + 1;
        return [buf.slice(start, start + len), start + len];
    }

    const [resp] = decode(body);
    const peers = [];

    if (resp.peers) {
        const peersBuf = Buffer.isBuffer(resp.peers)
            ? resp.peers
            : Buffer.from(resp.peers);

        // Compact format: 6 bytes per peer (4 IP + 2 port)
        for (let i = 0; i < peersBuf.length; i += 6) {
            const ip = `${peersBuf[i]}.${peersBuf[i+1]}.${peersBuf[i+2]}.${peersBuf[i+3]}`;
            const port = peersBuf.readUInt16BE(i + 4);
            if (port > 0 && port < 65536) peers.push({ ip, port });
        }
    }

    return peers;
}

// ── UDP helpers ────────────────────────────────────────────────────────────
export function udpSend(socket, message, rawUrl, callback = () => {}) {
    const url = URLParse(rawUrl);
    socket.send(message, 0, message.length, url.port, url.host, callback);
}

export function respType(resp) {
    const action = resp.readUInt32BE(0);
    if (action === 0) return 'connect';
    if (action === 1) return 'announce';
}

export function buildConnReq() {
    const buffer = Buffer.alloc(16);
    buffer.writeUInt32BE(0x417, 0);
    buffer.writeUInt32BE(0x27101980, 4);
    buffer.writeUInt32BE(0, 8);
    crypto.randomBytes(4).copy(buffer, 12);
    return buffer;
}

export function parseConnResp(resp) {
    return {
        action: resp.readUInt32BE(0),
        transactionId: resp.readUInt32BE(4),
        connectionId: resp.subarray(8)
    };
}

export function buildAnnounceReq(connId, torrent, port = 6881) {
    if (typeof port !== 'number' || port <= 0 || port >= 65536) {
        console.warn(`Invalid client port ${port}, falling back to 6881`);
        port = 6881;
    }

    const buffer = Buffer.allocUnsafe(98);
    connId.copy(buffer, 0);
    buffer.writeUInt32BE(1, 8);
    crypto.randomBytes(4).copy(buffer, 12);
    getInfoHash(torrent).copy(buffer, 16);
    generatePeerId().copy(buffer, 36);
    Buffer.alloc(8).copy(buffer, 56);
    size(torrent).copy(buffer, 64);
    Buffer.alloc(8).copy(buffer, 72);
    buffer.writeUInt32BE(0, 80);
    buffer.writeUInt32BE(0, 84);
    crypto.randomBytes(4).copy(buffer, 88);
    buffer.writeInt32BE(-1, 92);
    buffer.writeUInt16BE(port, 96);
    return buffer;
}

export function parseAnnounceResp(resp) {
    function group(iterable, groupSize) {
        let groups = [];
        for (let i = 0; i < iterable.length; i += groupSize) {
            groups.push(iterable.slice(i, i + groupSize));
        }
        return groups;
    }

    return {
        action: resp.readUInt32BE(0),
        transactionId: resp.readUInt32BE(4),
        leechers: resp.readUInt32BE(8),
        seeders: resp.readUInt32BE(12),
        peers: group(resp.slice(20), 6).map(address => {
            if (!address || address.length < 6) return null;
            const ip = address.slice(0, 4).join('.');
            const port = address.readUInt16BE(4);
            return port > 0 && port < 65536 ? { ip, port } : null;
        }).filter(Boolean)
    };
}