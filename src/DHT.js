/**
 * src/DHT.js — Distributed Hash Table client + node (BEP 5)
 *
 * Implements enough of the Kademlia-style DHT to:
 *   - bootstrap from well-known nodes
 *   - run an iterative `get_peers` lookup for an info hash
 *   - `announce_peer` to the closest responsive nodes
 *   - act as a first-class DHT node so other clients (and our own
 *     tests/instances) can query and announce to us
 *
 * Messages are bencoded per BEP 3; queries use transaction ids matched
 * against pending requests with a timeout.
 */

import dgram from 'dgram';
import crypto from 'crypto';
import bencode from 'bencode';
import { Buffer } from 'buffer';

// bencode v4 returns a raw Uint8Array — normalise everything to Buffer.
const enc = (...args) => Buffer.from(bencode.encode(...args));

// ── Constants ──────────────────────────────────────────────────────────────
export const K = 8;               // bucket size / max results
export const DEFAULT_DHT_PORT = 6882;

export const BOOTSTRAP_NODES = [
    'router.bittorrent.com:6881',
    'router.utorrent.com:6881',
    'dht.transmissionbt.com:6881',
];

// ── Address helpers ────────────────────────────────────────────────────────
export function randomNodeId() {
    return crypto.randomBytes(20);
}

export function xorDistance(a, b) {
    if (a.length !== b.length) throw new Error('node ids must be the same length');
    const out = Buffer.alloc(a.length);
    for (let i = 0; i < a.length; i++) out[i] = a[i] ^ b[i];
    return out;
}

// Ordering helper: is `a` closer to `target` than `b`?
export function isCloser(a, b, target) {
    return Buffer.compare(xorDistance(a, target), xorDistance(b, target)) < 0;
}

export function compactPeer(peer) {
    const buf = Buffer.alloc(6);
    const parts = String(peer.ip).split('.').map(Number);
    buf[0] = parts[0]; buf[1] = parts[1]; buf[2] = parts[2]; buf[3] = parts[3];
    buf.writeUInt16BE(peer.port, 4);
    return buf;
}

export function parseCompactPeers(buf) {
    const peers = [];
    for (let i = 0; i + 6 <= buf.length; i += 6) {
        const ip = `${buf[i]}.${buf[i + 1]}.${buf[i + 2]}.${buf[i + 3]}`;
        const port = buf.readUInt16BE(i + 4);
        if (port > 0 && port < 65536) peers.push({ ip, port });
    }
    return peers;
}

export function compactNode(node) {
    const buf = Buffer.alloc(26);
    const parts = String(node.ip).split('.').map(Number);
    buf[0] = parts[0]; buf[1] = parts[1]; buf[2] = parts[2]; buf[3] = parts[3];
    buf.writeUInt16BE(node.port, 4);
    node.id.copy(buf, 6);
    return buf;
}

export function parseCompactNodes(buf) {
    const nodes = [];
    for (let i = 0; i + 26 <= buf.length; i += 26) {
        const ip = `${buf[i]}.${buf[i + 1]}.${buf[i + 2]}.${buf[i + 3]}`;
        const port = buf.readUInt16BE(i + 4);
        const id = Buffer.from(buf.subarray(i + 6, i + 26));
        if (port > 0 && port < 65536) nodes.push({ ip, port, id });
    }
    return nodes;
}

// ── Query builders ─────────────────────────────────────────────────────────
export function buildPingQuery(nodeId) {
    return enc({
        t: crypto.randomBytes(2).toString('hex'),
        y: 'q',
        q: 'ping',
        a: { id: nodeId },
    });
}

export function buildGetPeersQuery(nodeId, infoHash) {
    return enc({
        t: crypto.randomBytes(2).toString('hex'),
        y: 'q',
        q: 'get_peers',
        a: { id: nodeId, info_hash: infoHash },
    });
}

export function buildAnnouncePeerQuery(nodeId, infoHash, port, token) {
    return enc({
        t: crypto.randomBytes(2).toString('hex'),
        y: 'q',
        q: 'announce_peer',
        a: { id: nodeId, info_hash: infoHash, port, token },
    });
}

// ── Response builders (server side) ────────────────────────────────────────
export function buildResponse(txId, reply) {
    return enc({ t: txId, y: 'r', r: reply });
}

export function buildErrorResponse(txId, code, message) {
    return enc({ t: txId, y: 'e', e: [code, message] });
}

// ── Message parsing ────────────────────────────────────────────────────────
export function parseDhtMessage(buf) {
    // Buffer-mode decode: binary fields (info_hash, ids) are not valid UTF-8
    const msg = bencode.decode(buf);
    // Buffer-mode decode: ids/hashes are not valid UTF-8, but short ASCII
    // fields (y, q) also come back as Uint8Array and need conversion.
    const toStr = (v) => (v instanceof Uint8Array || v instanceof Buffer)
        ? Buffer.from(v).toString('utf8')
        : String(v);
    return {
        t: (msg.t instanceof Uint8Array || msg.t instanceof Buffer)
            ? Buffer.from(msg.t)
            : Buffer.from(String(msg.t ?? ''), 'utf8'),
        y: msg.y !== undefined ? toStr(msg.y) : undefined,
        q: msg.q !== undefined ? toStr(msg.q) : undefined,
        a: msg.a ? normaliseDict(msg.a) : undefined,
        r: msg.r ? normaliseDict(msg.r) : undefined,
        e: msg.e,
    };
}

function normaliseDict(dict) {
    const out = {};
    for (const [k, v] of Object.entries(dict)) {
        if (v instanceof Uint8Array) out[k] = Buffer.from(v);
        else if (Array.isArray(v)) out[k] = v.map(item =>
            item instanceof Uint8Array ? Buffer.from(item) : item);
        else if (v !== null && typeof v === 'object') out[k] = normaliseDict(v);
        else out[k] = v;
    }
    return out;
}

// ── The node ───────────────────────────────────────────────────────────────
/**
 * A DHT node: UDP server + routing table + client operations.
 * Both roles live in one socket: we answer incoming queries and we send
 * our own queries on the same socket.
 */
export default class DHTNode {
    constructor({ port = DEFAULT_DHT_PORT, bootstrap = null, log = () => {} } = {}) {
        this.port = port;
        this.nodeId = randomNodeId();
        this.log = log;
        this.bootstrapHosts = bootstrap ?? BOOTSTRAP_NODES;

        // Routing table — simplified flat store keyed by "ip:port"
        this.table = new Map();        // addr → { id, ip, port, token, lastSeen }
        this.swarms = new Map();       // infoHashHex → Map("ip:port" → {ip, port})

        this.pending = new Map();      // txId hex → { resolve, timer }
        this.txCounter = 0;
        this.socket = null;
        this.started = false;
    }

    start() {
        return new Promise((resolve) => {
            this.socket = dgram.createSocket('udp4');

            this.socket.on('message', (data, rinfo) => this._onMessage(data, rinfo));
            this.socket.on('error', (err) => {
                this.log(`DHT socket error: ${err.message}`);
                try { this.socket.close(); } catch (_) {}
                this.socket = null;
            });
            this.socket.on('listening', () => {
                const { port } = this.socket.address();
                this.port = port;
                this.started = true;
                resolve(port);
            });

            this.socket.bind(this.port);
        });
    }

    close() {
        for (const { timer } of this.pending.values()) clearTimeout(timer);
        this.pending.clear();
        if (this.socket && this.listening()) {
            try { this.socket.close(); } catch (_) {}
        }
        this.socket = null;
        this.started = false;
    }

    listening() {
        try {
            return this.socket && this.socket.address().port > 0;
        } catch (_) {
            return false;
        }
    }

    // ── server side ────────────────────────────────────────────────────────
    _onMessage(data, rinfo) {
        let msg;
        try {
            msg = parseDhtMessage(data);
        } catch (_) {
            return; // malformed — ignore
        }

        if (msg.y === 'q') {
            this._handleQuery(msg, rinfo);
        } else if (msg.y === 'r') {
            const tx = msg.t.toString('hex');
            const pendingRequest = this.pending.get(tx);
            if (pendingRequest) {
                clearTimeout(pendingRequest.timer);
                this.pending.delete(tx);
                pendingRequest.resolve({ msg, rinfo });
            }
        }
        // 'e' responses resolve their pending lookup as empty (already timed
        // out there); silently ignore here.

        // Learn about the sender (BEP 5 says we can trust the ip: from rinfo)
        if (msg.r?.id || msg.a?.id) {
            this._learnNode(rinfo.address, rinfo.port, msg.r?.id ?? msg.a?.id);
        }
    }

    _handleQuery(msg, rinfo) {
        const txId = msg.t;
        const ourId = this.nodeId;
        const a = msg.a ?? {};

        // We only speak to four-byte IPv4 peers.
        if (!rinfo.address.includes('.')) {
            return;
        }

        if (msg.q === 'ping') {
            this._send(buildResponse(txId, { id: ourId }), rinfo);
        } else if (msg.q === 'get_peers') {
            const infoHash = a.info_hash;
            if (!infoHash || infoHash.length !== 20) {
                return this._send(buildErrorResponse(txId, 203, 'bad info_hash'), rinfo);
            }
            const token = this._tokenFor(rinfo.address, rinfo.port);
            const swarm = this.swarms.get(infoHash.toString('hex'));
            const reply = { id: ourId, token };

            if (swarm && swarm.size > 0) {
                reply.values = [...swarm.entries()].map(([, peer]) => compactPeer(peer));
            }
            const nodes = this.closestNodes(infoHash).map(compactNode);
            if (nodes.length) {
                reply.nodes = Buffer.concat(nodes);
            }
            this._send(buildResponse(txId, reply), rinfo);
        } else if (msg.q === 'announce_peer') {
            const tokenValid = this._checkToken(rinfo.address, rinfo.port, a.token);
            if (!tokenValid) {
                return this._send(buildErrorResponse(txId, 203, 'bad token'), rinfo);
            }
            const infoHash = a.info_hash;
            if (!infoHash || infoHash.length !== 20) {
                return this._send(buildErrorResponse(txId, 203, 'bad info_hash'), rinfo);
            }
            // implied_port is 1 → use the sender's source port instead of a.port
            const port = (a.implied_port === 1) ? rinfo.port : a.port;
            if (port <= 0 || port >= 65536) {
                return this._send(buildErrorResponse(txId, 203, 'bad port'), rinfo);
            }
            const peer = { ip: rinfo.address, port };
            const key = infoHash.toString('hex');
            if (!this.swarms.has(key)) this.swarms.set(key, new Map());
            this.swarms.get(key).set(`${peer.ip}:${peer.port}`, peer);
            this._send(buildResponse(txId, { id: ourId }), rinfo);
        } else {
            this._send(buildErrorResponse(txId, 204, 'unknown method'), rinfo);
        }
    }

    _send(buf, rinfo) {
        if (!this.socket || !this.listening()) return;
        try {
            this.socket.send(buf, 0, buf.length, rinfo.port, rinfo.address);
        } catch (_) {}
    }

    // Per-BEP 5 the token is opaque; derive one deterministically per
    // address+port so validators can check it.
    _tokenFor(ip, port) {
        const h = crypto.createHash('sha1');
        h.update(`${this.nodeId.toString('hex')}|${ip}|${port}`);
        return h.digest().subarray(0, 8);
    }

    _checkToken(ip, port, token) {
        if (!token || token.length !== 8) return false;
        return this._tokenFor(ip, port).equals(token);
    }

    _learnNode(ip, port, id) {
        if (!ip.includes('.') || port <= 0 || port >= 65536 || !id || id.length !== 20) return;
        const key = `${ip}:${port}`;
        const token = this._tokenFor(ip, port);
        const existing = this.table.get(key);
        if (existing) {
            existing.lastSeen = Date.now();
            existing.id = id;
            existing.token = token;
        } else {
            this.table.set(key, { id: Buffer.from(id), ip, port, token, lastSeen: Date.now() });
        }
    }

    closestNodes(infoHash) {
        return [...this.table.values()]
            .sort((n1, n2) => Buffer.compare(
                xorDistance(n1.id, infoHash), xorDistance(n2.id, infoHash)))
            .slice(0, K);
    }

    // ── client side ────────────────────────────────────────────────────────
    _nextTxId() {
        this.txCounter = (this.txCounter + 1) & 0xffff;
        return this.txCounter.toString(16).padStart(4, '0');
    }

    _query(buf, host, port, ownTimeoutMs = 3000) {
        const txHex = parseDhtMessage(buf).t.toString('hex');
        return new Promise((resolve) => {
            const rec = {
                resolve,
                timer: setTimeout(() => {
                    this.pending.delete(txHex);
                    resolve(null);
                }, ownTimeoutMs),
            };
            this.pending.set(txHex, rec);
            this.socket.send(buf, 0, buf.length, port, host, (err) => {
                if (err) {
                    clearTimeout(rec.timer);
                    this.pending.delete(txHex);
                    resolve(null);
                }
            });
        });
    }

    /** Ask bootstrap nodes for our own id (also seeds the routing table). */
    async bootstrap() {
        const hosts = this.bootstrapHosts.map(h => {
            const [host, port] = h.split(':');
            return { host, port: Number(port) || 6881 };
        });
        await Promise.all(hosts.map(h =>
            this._query(buildPingQuery(this.nodeId), h.host, h.port)));
        return this.table.size;
    }

    /**
     * Iterative get_peers lookup per BEP 5.
     * Returns { peers: [{ip, port}], announced: boolean }.
     */
    async getPeers(infoHash, {
        timeoutMs = 10000,
        announcePort = null,
        maxPeers = 50,
    } = {}) {
        const target = Buffer.from(infoHash);
        const visited = new Set();
        const candidates = [...this.table.values()];
        const peers = new Map();
        const tokens = []; // {ip, port, token} for announce_peer
        const deadline = Date.now() + timeoutMs;
        const CONCURRENCY = 8;

        while (Date.now() < deadline && peers.size < maxPeers) {
            // Pick unvisited candidates closest to the target
            const batch = candidates
                .filter(n => !visited.has(`${n.ip}:${n.port}`))
                .slice(0, CONCURRENCY);

            if (batch.length === 0) break;

            const responses = await Promise.all(batch.map(node => {
                visited.add(`${node.ip}:${node.port}`);
                return this._query(buildGetPeersQuery(this.nodeId, target), node.ip, node.port);
            }));

            for (const resp of responses) {
                if (!resp) continue;
                const r = resp.msg.r ?? {};

                if (r.token && r.values) {
                    tokens.push({ ip: resp.rinfo.address, port: resp.rinfo.port, token: r.token });
                    for (const value of r.values) {
                        for (const peer of parseCompactPeers(value)) {
                            peers.set(`${peer.ip}:${peer.port}`, peer);
                        }
                    }
                } else if (r.token && !r.nodes) {
                    // node has token but knows nothing yet — still announceable
                    tokens.push({ ip: resp.rinfo.address, port: resp.rinfo.port, token: r.token });
                }

                if (r.id) this._learnNode(resp.rinfo.address, resp.rinfo.port, r.id);
                if (r.nodes) {
                    const parsed = parseCompactNodes(r.nodes);
                    for (const n of parsed) {
                        this._learnNode(n.ip, n.port, n.id);
                        candidates.push(n);
                    }
                    // Keep the routing candidates bounded & closest-first
                    candidates.sort((n1, n2) => Buffer.compare(
                        xorDistance(n1.id, target), xorDistance(n2.id, target)));
                    if (candidates.length > 128) candidates.length = 128;
                }
            }

            if (peers.size > 0 && visited.size >= K * 2) break;
        }

        // Announce ourselves to the K closest announceable nodes
        let announced = false;
        if (announcePort != null && tokens.length > 0) {
            const targets = tokens.slice(0, K);
            await Promise.all(targets.map(t =>
                this._query(
                    buildAnnouncePeerQuery(this.nodeId, target, announcePort, t.token),
                    t.ip, t.port
                )));
            announced = targets.length > 0;
        }

        return { peers: [...peers.values()], announced };
    }
}
