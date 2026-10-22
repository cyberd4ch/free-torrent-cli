#!/usr/bin/env node
/**
 * server.js — Headless web UI for free-torrent-cli.
 *
 * Runs the download engine with no display required (NAS, VPS, Raspberry
 * Pi). Zero runtime dependencies beyond what the CLI already pulls in:
 *
 *   HTTP  → dashboard + REST API
 *   SSE   → live status streaming (server-sent events, no WebSocket dep)
 *
 * API:
 *   GET  /                 dashboard
 *   GET  /app.js           dashboard logic
 *   GET  /api/torrents     current snapshot list
 *   GET  /api/events       SSE stream (add/update/remove/info)
 *   POST /api/add          { magnet: "magnet:?..." } | { torrentPath: "..." }
 *                          optional outputDir; returns { id } or { error }
 *   POST /api/remove       { id }
 */

import http from 'http';
import fs from 'fs';
import path from 'path';
import url from 'url';
import os from 'os';
import bencode from 'bencode';
import { resolveMagnet } from './src/Magnet.js';
import startTorrentDownload from './src/engine.js';

const __dirname = path.dirname(url.fileURLToPath(import.meta.url));
const WEB_DIR = path.join(__dirname, 'web');
const DEFAULT_PORT = 9090;

// ── state ──────────────────────────────────────────────────────────────────
/** id → { info: {...meta}, engine: {promise, cancel} | null, resolving: bool } */
const torrents = new Map();
/** id → latest status snapshot */
const lastStatus = new Map();
const sseClients = new Map(); // id → ServerResponse

// bencode v4 decodes to plain Uint8Arrays; downstream code expects Buffers
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

function nextId() {
    return Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
}

function loadDefaultOutputDir() {
    const args = process.argv.slice(2);
    const i = args.indexOf('--output');
    if (i !== -1 && args[i + 1]) return path.resolve(args[i + 1]);
    return path.join(os.homedir(), 'Downloads');
}

function broadcast(event, data) {
    const payload = `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
    for (const res of sseClients.values()) {
        try { res.write(payload); } catch (_) {}
    }
}

function summaryList() {
    return [...torrents.entries()].map(([id, t]) => ({
        id,
        ...t.info,
        ...(lastStatus.get(id) ?? {}),
        resolving: t.resolving,
    }));
}

// ── torrent lifecycle ──────────────────────────────────────────────────────
function runTorrent(id, torrent, outputDir) {
    const entry = torrents.get(id);
    const engine = startTorrentDownload({
        torrent,
        outputDir,
        onUpdate: (snapshot) => {
            lastStatus.set(id, snapshot);
            broadcast('update', { id, ...snapshot });
        },
    });
    entry.engine = engine;

    engine.promise.then(({ done, error } = {}) => {
        if (!cancelled(id)) {
            const final = { ...(lastStatus.get(id) ?? {}) , status: cancelled(id) ? undefined : (error ? 'error' : 'seeding') };
            if (error) final.error = error;
            lastStatus.set(id, final);
            broadcast('update', { id, ...final });
        }
    });
}

function cancelled(id) {
    return !torrents.has(id);
}

function addTorrent({ torrentPath, magnet, outputDir }) {
    const id = magnet ? readMagnetId(magnet) : nextId();
    if (!id) return { error: 'invalid magnet URI' };
    if (torrents.has(id)) return { error: 'already added' };

    const resolvedDir = outputDir ? path.resolve(outputDir) : loadDefaultOutputDir();

    const info = {
        name: magnet ? 'resolving magnet…' : path.basename(String(torrentPath)),
        size: null,
        numPieces: null,
        pieceLength: null,
        isMultiFile: false,
        files: [],
        outputDir: resolvedDir,
        status: magnet ? 'resolving' : 'loading',
        addedAt: Date.now(),
    };
    torrents.set(id, { info, engine: null, resolving: !!magnet });
    lastStatus.set(id, { status: info.status, progress: 0 });
    broadcast('added', { id, ...info });
    broadcast('update', { id, ...lastStatus.get(id) });

    (async () => {
        try {
            let torrent;
            if (magnet) {
                torrent = await resolveMagnet(magnet, {
                    workshopPort: 6881,
                    onStatus: msg => broadcast('log', { id, msg }),
                    onMetadataProgress: n => {
                        lastStatus.set(id, { status: 'resolving', metadataPieces: n, progress: 0 });
                        broadcast('update', { id, ...lastStatus.get(id) });
                    },
                });
                updateInfoFromTorrent(id, torrent);
            } else {
                const data = await fs.promises.readFile(String(torrentPath));
                torrent = normalise(bencode.decode(data));
                updateInfoFromTorrent(id, torrent);
            }
            torrents.get(id).resolving = false;
            runTorrent(id, torrent, resolvedDir);
        } catch (err) {
            lastStatus.set(id, { status: 'error', error: err.message, progress: 0 });
            broadcast('update', { id, ...lastStatus.get(id) });
        }
    })().catch(err => {
        lastStatus.set(id, { status: 'error', error: err.message });
        broadcast('update', { id, ...lastStatus.get(id) });
    });

    return { id };
}

function updateInfoFromTorrent(id, torrent) {
    const entry = torrents.get(id);
    if (!entry) return;
    const files = torrent.info.files
        ? torrent.info.files.map(f => ({
            name: f.path.map(p => (Buffer.isBuffer(p) ? p.toString('utf8') : String(p))).join('/'),
            size: f.length,
        }))
        : [{ name: torrent.info.name.toString('utf8'), size: torrent.info.length }];
    entry.info = {
        ...entry.info,
        name: torrent.info.name.toString('utf8'),
        size: files.reduce((s, f) => s + f.size, 0),
        numPieces: Math.ceil(torrent.info.pieces.length / 20),
        pieceLength: torrent.info['piece length'],
        isMultiFile: !!torrent.info.files,
        files,
    };
    broadcast('added', { id, ...entry.info });
}

function readMagnetId(magnet) {
    try {
        // deterministic id: anything after "urn:btih:", trimmed safe
        const m = /xt=urn:btih:([^&]+)/.exec(String(magnet));
        return m ? encodeURIComponent(m[1].slice(0, 40).toLowerCase()) : nextId();
    } catch (_) {
        return nextId();
    }
}

function removeTorrent(id) {
    const entry = torrents.get(id);
    if (!entry) return { success: false, error: 'unknown id' };
    entry.engine?.cancel();
    torrents.delete(id);
    lastStatus.delete(id);
    broadcast('removed', { id });
    return { success: true };
}

// ── HTTP plumbing ──────────────────────────────────────────────────────────
function serveStatic(req, res) {
    const rel = req.url === '/' ? '/index.html' : req.url.split('?')[0];
    const abs = path.join(WEB_DIR, path.normalize(rel).replace(/^(\.\.[/\\])+/, ''));
    if (!abs.startsWith(WEB_DIR)) {
        res.writeHead(403); res.end('forbidden'); return;
    }
    fs.readFile(abs, (err, data) => {
        if (err) {
            res.writeHead(404, { 'Content-Type': 'text/plain' });
            res.end('not found');
            return;
        }
        const types = {
            '.html': 'text/html; charset=utf-8',
            '.js': 'text/javascript; charset=utf-8',
            '.css': 'text/css; charset=utf-8',
            '.svg': 'image/svg+xml',
        };
        res.writeHead(200, { 'Content-Type': types[path.extname(abs)] ?? 'application/octet-stream' });
        res.end(data);
    });
}

function readBody(req) {
    return new Promise((resolve) => {
        let body = '';
        req.on('data', c => { body += c; if (body.length > 1e6) req.destroy(); });
        req.on('end', () => {
            try { resolve(JSON.parse(body || '{}')); } catch (_) { resolve({}); }
        });
        req.on('error', () => resolve({}));
    });
}

const server = http.createServer(async (req, res) => {
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Access-Control-Allow-Headers', 'Content-Type');

    const route = req.url.split('?')[0];

    if (req.method === 'GET' && route === '/api/events') {
        res.writeHead(200, {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
            Connection: 'keep-alive',
        });
        res.write(`data: ${JSON.stringify({ type: 'snapshot', torrents: summaryList() })}\n\n`);
        const clientId = nextId();
        sseClients.set(clientId, res);
        req.on('close', () => sseClients.delete(clientId));
        return;
    }

    if (req.method === 'GET' && route === '/api/torrents') {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(summaryList()));
        return;
    }

    if (req.method === 'GET' && route === '/api/info') {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ name: 'free-torrent-cli server', uptime: process.uptime() }));
        return;
    }

    if (req.method === 'POST' && route === '/api/add') {
        const body = await readBody(req);
        if (!body.magnet && !body.torrentPath) {
            res.writeHead(400, { 'Content-Type': 'application/json' });
            res.end(JSON.stringify({ error: 'need "magnet" or "torrentPath"' }));
            return;
        }
        const result = addTorrent(body);
        res.writeHead(result.error ? 400 : 200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(result));
        return;
    }

    if (req.method === 'POST' && route === '/api/remove') {
        const body = await readBody(req);
        const result = removeTorrent(String(body.id ?? ''));
        res.writeHead(result.success ? 200 : 404, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(result));
        return;
    }

    if (req.method === 'GET' || req.method === 'HEAD') {
        return serveStatic(req, res);
    }

    res.writeHead(404, { 'Content-Type': 'text/plain' });
    res.end('not found');
});

const PORT_ARG = process.argv.indexOf('--port');
const HOST_ARG = process.argv.indexOf('--host');
const PORT_ARG_VAL = PORT_ARG !== -1 ? Number(process.argv[PORT_ARG + 1]) : DEFAULT_PORT;
// Port 0 means “OS, pick an ephemeral one” — keep that meaning instead of
// falling back to the default like Number(0)||DEFAULT would do.
const PORT = PORT_ARG !== -1
    ? (Number.isInteger(PORT_ARG_VAL) ? PORT_ARG_VAL : 0)
    : DEFAULT_PORT;
const HOST = HOST_ARG !== -1 ? process.argv[HOST_ARG + 1] : '127.0.0.1';

server.listen(PORT, HOST, () => {
    const bound = server.address().port;
    console.log('');
    console.log('  free-torrent-cli web server');
    console.log(`  Dashboard : http://${HOST}:${bound}`);
    console.log(`  Output dir: ${loadDefaultOutputDir()}`);
    console.log('');
});

process.on('SIGINT', () => {
    for (const t of torrents.values()) t.engine?.cancel();
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 500);
});
