/**
 * free-torrent-cli — unit test suite
 * Run with:  node tests/run.js
 */

import crypto from 'crypto';
import path from 'path';
import { fileURLToPath } from 'url';
import { Buffer } from 'buffer';

function sleep(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
}

// ── tiny test harness ──────────────────────────────────────────────────────
let passed = 0, failed = 0;

function test(name, fn) {
    try {
        fn();
        console.log(`  ✅  ${name}`);
        passed++;
    } catch (e) {
        console.error(`  ❌  ${name}`);
        console.error(`      ${e.message}`);
        failed++;
    }
}

function assert(cond, msg) {
    if (!cond) throw new Error(msg || 'Assertion failed');
}

function assertEqual(a, b, msg) {
    if (a !== b) throw new Error(msg || `Expected ${b}, got ${a}`);
}

// Async tests: await at their call site (top-level await is fine here).
async function testA(name, fn) {
    try {
        await fn();
        console.log(`  ✅  ${name}`);
        passed++;
    } catch (e) {
        console.error(`  ❌  ${name}`);
        console.error(`      ${e.message}`);
        failed++;
    }
}

// ── imports ────────────────────────────────────────────────────────────────
import { generatePeerId } from '../src/util.js';
import { size, getInfoHash } from '../src/TorrentParser.js';
import {
    buildConnReq, buildAnnounceReq, respType,
    parseConnResp, parseAnnounceResp
} from '../src/tracker.js';
import {
    buildHandshake, buildInterested, buildChoke, buildUnchoke,
    buildHave, buildRequest, buildCancel, buildPort,
    buildKeepAlive, buildPiece, parseMessage
} from '../src/Message.js';
import Pieces from '../src/Pieces.js';

// ══════════════════════════════════════════════════════════════════════════
// 1. util.js
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── util.js ──────────────────────────────────────────────────');

test('generatePeerId returns a 20-byte Buffer', () => {
    const id = generatePeerId();
    assert(Buffer.isBuffer(id), 'not a Buffer');
    assertEqual(id.length, 20, 'wrong length');
});

test('generatePeerId is stable (same reference across calls)', () => {
    const a = generatePeerId();
    const b = generatePeerId();
    assert(a === b, 'should return the same Buffer instance');
});

test('generatePeerId starts with client prefix -TR2930-', () => {
    const id = generatePeerId();
    assertEqual(id.toString('utf8', 0, 8), '-TR2930-', 'prefix mismatch');
});

// ══════════════════════════════════════════════════════════════════════════
// 2. TorrentParser.js
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── TorrentParser.js ─────────────────────────────────────────');

// Minimal single-file torrent mock
const singleFileTorrent = {
    info: {
        name: Buffer.from('test.txt'),
        length: 1024,
        'piece length': 512,
        pieces: crypto.randomBytes(40) // 2 pieces
    }
};

// Minimal multi-file torrent mock
const multiFileTorrent = {
    info: {
        name: Buffer.from('mydir'),
        'piece length': 512,
        pieces: crypto.randomBytes(40),
        files: [
            { length: 500, path: [Buffer.from('a.txt')] },
            { length: 524, path: [Buffer.from('b.txt')] }
        ]
    }
};

test('size() returns an 8-byte Buffer for single-file torrent', () => {
    const s = size(singleFileTorrent);
    assert(Buffer.isBuffer(s), 'not a Buffer');
    assertEqual(s.length, 8, 'wrong length');
});

test('size() encodes correct value for single-file torrent (1024 bytes)', () => {
    const s = size(singleFileTorrent);
    assertEqual(s.readBigUInt64BE(0), 1024n, 'wrong encoded size');
});

test('size() sums files correctly for multi-file torrent (500 + 524 = 1024)', () => {
    const s = size(multiFileTorrent);
    assertEqual(s.readBigUInt64BE(0), 1024n, 'wrong summed size');
});

test('getInfoHash returns a 20-byte SHA-1 digest', () => {
    const hash = getInfoHash(singleFileTorrent);
    assert(Buffer.isBuffer(hash), 'not a Buffer');
    assertEqual(hash.length, 20, 'wrong length');
});

test('getInfoHash is deterministic for the same torrent', () => {
    const h1 = getInfoHash(singleFileTorrent);
    const h2 = getInfoHash(singleFileTorrent);
    assert(h1.equals(h2), 'hash is not deterministic');
});

// ══════════════════════════════════════════════════════════════════════════
// 3. tracker.js
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── tracker.js ───────────────────────────────────────────────');

test('buildConnReq returns 16-byte Buffer', () => {
    const req = buildConnReq();
    assert(Buffer.isBuffer(req), 'not a Buffer');
    assertEqual(req.length, 16, 'wrong length');
});

test('buildConnReq has correct magic connection id (0x41727101980)', () => {
    const req = buildConnReq();
    assertEqual(req.readUInt32BE(0), 0x417, 'magic hi dword wrong');
    assertEqual(req.readUInt32BE(4), 0x27101980, 'magic lo dword wrong');
});

test('buildConnReq action field is 0 (connect)', () => {
    const req = buildConnReq();
    assertEqual(req.readUInt32BE(8), 0, 'action should be 0');
});

test('parseConnResp correctly parses a synthetic connect response', () => {
    const resp = Buffer.alloc(16);
    resp.writeUInt32BE(0, 0);   // action = connect
    resp.writeUInt32BE(42, 4);  // transaction id
    crypto.randomBytes(8).copy(resp, 8); // connection id
    const parsed = parseConnResp(resp);
    assertEqual(parsed.action, 0, 'wrong action');
    assertEqual(parsed.transactionId, 42, 'wrong txId');
    assertEqual(parsed.connectionId.length, 8, 'wrong connId length');
});

test('respType returns "connect" for action 0', () => {
    const buf = Buffer.alloc(4);
    buf.writeUInt32BE(0, 0);
    assertEqual(respType(buf), 'connect');
});

test('respType returns "announce" for action 1', () => {
    const buf = Buffer.alloc(4);
    buf.writeUInt32BE(1, 0);
    assertEqual(respType(buf), 'announce');
});

test('buildAnnounceReq returns 98-byte Buffer', () => {
    const connId = crypto.randomBytes(8);
    const req = buildAnnounceReq(connId, singleFileTorrent, 6881);
    assertEqual(req.length, 98, 'wrong length');
});

test('buildAnnounceReq action field is 1 (announce)', () => {
    const connId = crypto.randomBytes(8);
    const req = buildAnnounceReq(connId, singleFileTorrent, 6881);
    assertEqual(req.readUInt32BE(8), 1, 'action should be 1');
});

test('buildAnnounceReq port is written at offset 96', () => {
    const connId = crypto.randomBytes(8);
    const req = buildAnnounceReq(connId, singleFileTorrent, 6881);
    assertEqual(req.readUInt16BE(96), 6881, 'wrong port');
});

test('buildAnnounceReq IP offset (84) and event offset (80) are distinct', () => {
    const connId = Buffer.alloc(8);
    const req = buildAnnounceReq(connId, singleFileTorrent, 6881);
    // event at 80 should be 0, IP at 84 should be 0 — both 0 but at different offsets
    assertEqual(req.readUInt32BE(80), 0, 'event not 0');
    assertEqual(req.readUInt32BE(84), 0, 'ip not 0');
});

test('parseAnnounceResp filters out invalid peers', () => {
    // Build a synthetic announce response with one valid and one port=0 peer
    const resp = Buffer.alloc(26); // 20 header + 6 peer
    resp.writeUInt32BE(1, 0);  // action = announce
    // peer: 127.0.0.1:6881
    resp[20] = 127; resp[21] = 0; resp[22] = 0; resp[23] = 1;
    resp.writeUInt16BE(6881, 24);
    const result = parseAnnounceResp(resp);
    assertEqual(result.peers.length, 1, 'should have 1 valid peer');
    assertEqual(result.peers[0].ip, '127.0.0.1', 'wrong IP');
    assertEqual(result.peers[0].port, 6881, 'wrong port');
});

test('parseAnnounceResp drops peers with port 0', () => {
    const resp = Buffer.alloc(26);
    resp.writeUInt32BE(1, 0);
    resp.writeUInt16BE(0, 24); // port = 0 → invalid
    const result = parseAnnounceResp(resp);
    assertEqual(result.peers.length, 0, 'should filter out port-0 peer');
});

// ══════════════════════════════════════════════════════════════════════════
// 4. Message.js
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── Message.js ───────────────────────────────────────────────');

test('buildHandshake returns 68-byte Buffer', () => {
    const hs = buildHandshake(singleFileTorrent);
    assertEqual(hs.length, 68, 'wrong length');
});

test('buildHandshake pstrlen is 19', () => {
    const hs = buildHandshake(singleFileTorrent);
    assertEqual(hs.readUInt8(0), 19, 'pstrlen should be 19');
});

test('buildHandshake protocol string is "BitTorrent protocol"', () => {
    const hs = buildHandshake(singleFileTorrent);
    assertEqual(hs.toString('utf8', 1, 20), 'BitTorrent protocol');
});

test('buildKeepAlive returns 4-byte zero Buffer', () => {
    const ka = buildKeepAlive();
    assertEqual(ka.length, 4);
    assert(ka.every(b => b === 0), 'keep-alive must be all zeros');
});

test('buildChoke: length=1, id=0', () => {
    const msg = buildChoke();
    assertEqual(msg.readUInt32BE(0), 1);
    assertEqual(msg.readUInt8(4), 0);
});

test('buildUnchoke: length=1, id=2', () => {
    const msg = buildUnchoke();
    assertEqual(msg.readUInt32BE(0), 1);
    assertEqual(msg.readUInt8(4), 2);
});

test('buildInterested: length=1, id=3', () => {
    const msg = buildInterested();
    assertEqual(msg.readUInt32BE(0), 1);
    assertEqual(msg.readUInt8(4), 3);
});

test('buildHave: correct length, id=4, correct piece index', () => {
    const msg = buildHave(7);
    assertEqual(msg.readUInt32BE(0), 5);
    assertEqual(msg.readUInt8(4), 4);
    assertEqual(msg.readUInt32BE(5), 7);
});

test('buildRequest: returns 17-byte Buffer', () => {
    const msg = buildRequest({ index: 0, begin: 0, length: 16384 });
    assertEqual(msg.length, 17, 'must be 17 bytes');
});

test('buildRequest: id=6, correct index/begin/length', () => {
    const msg = buildRequest({ index: 3, begin: 16384, length: 16384 });
    assertEqual(msg.readUInt8(4), 6, 'id should be 6');
    assertEqual(msg.readUInt32BE(5), 3, 'wrong index');
    assertEqual(msg.readUInt32BE(9), 16384, 'wrong begin');
    assertEqual(msg.readUInt32BE(13), 16384, 'wrong length');
});

test('buildCancel: 17-byte Buffer, id=8', () => {
    const msg = buildCancel({ index: 1, begin: 0, length: 16384 });
    assertEqual(msg.length, 17);
    assertEqual(msg.readUInt8(4), 8);
});

test('buildPort: 7-byte Buffer, id=9, correct port', () => {
    const msg = buildPort(51413);
    assertEqual(msg.length, 7);
    assertEqual(msg.readUInt8(4), 9);
    assertEqual(msg.readUInt16BE(5), 51413);
});

test('parseMessage: keep-alive (length=0) returns id=null', () => {
    const msg = Buffer.alloc(4); // length prefix = 0
    const parsed = parseMessage(msg);
    assertEqual(parsed.id, null);
    assertEqual(parsed.payload, null);
});

test('parseMessage: choke (id=0) parsed correctly', () => {
    const msg = buildChoke();
    const parsed = parseMessage(msg);
    assertEqual(parsed.id, 0);
});

test('parseMessage: have (id=4) parsed correctly', () => {
    const raw = buildHave(99);
    const parsed = parseMessage(raw);
    assertEqual(parsed.id, 4);
    assertEqual(parsed.payload.readUInt32BE(0), 99);
});

test('parseMessage: piece (id=7) splits index/begin/block', () => {
    const block = Buffer.from('hello world data');
    const raw = buildPiece({ index: 2, begin: 0, block });
    const parsed = parseMessage(raw);
    assertEqual(parsed.id, 7);
    assertEqual(parsed.payload.index, 2);
    assertEqual(parsed.payload.begin, 0);
    assert(parsed.payload.block.equals(block), 'block data mismatch');
});

// ══════════════════════════════════════════════════════════════════════════
// 5. Pieces.js
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── Pieces.js ────────────────────────────────────────────────');

test('new Pieces(n): needed() returns true for all pieces initially', () => {
    const p = new Pieces(5);
    for (let i = 0; i < 5; i++) {
        assert(p.needed(i), `piece ${i} should be needed`);
    }
});

test('addRequested: piece is no longer needed', () => {
    const p = new Pieces(5);
    p.addRequested(2);
    assert(!p.needed(2), 'requested piece should not be needed');
});

test('addReceived: piece is fully done, removed from requested', () => {
    const p = new Pieces(5);
    p.addRequested(2);
    p.addReceived(2);
    assert(!p.needed(2), 'received piece should not be needed');
    assertEqual(p.received.size, 1);
    assertEqual(p.requested.size, 0, 'should be removed from requested');
});

test('remaining decreases as pieces are received', () => {
    const p = new Pieces(4);
    assertEqual(p.remaining, 4);
    p.addReceived(0);
    assertEqual(p.remaining, 3);
    p.addReceived(1);
    assertEqual(p.remaining, 2);
});

test('isDone() returns false until all pieces received', () => {
    const p = new Pieces(3);
    p.addReceived(0);
    p.addReceived(1);
    assert(!p.isDone(), 'should not be done with 1 piece left');
});

test('isDone() returns true when all pieces are received', () => {
    const p = new Pieces(3);
    p.addReceived(0);
    p.addReceived(1);
    p.addReceived(2);
    assert(p.isDone(), 'should be done');
});

test('isDone() works correctly for single-piece torrent', () => {
    const p = new Pieces(1);
    assert(!p.isDone());
    p.addReceived(0);
    assert(p.isDone());
});




// ══════════════════════════════════════════════════════════════════════════
// 6. FileManager.js
// ══════════════════════════════════════════════════════════════════════════
import FileManager from '../src/FileManager.js';
import os from 'os';

console.log('\n── FileManager.js ───────────────────────────────────────────');

const tmpDir = os.tmpdir();

// Single-file torrent mock
const sfTorrent = {
    info: {
        name: Buffer.from('test-single.txt'),
        length: 1024,
        'piece length': 512,
        pieces: crypto.randomBytes(40)
    }
};

// Multi-file torrent mock (2 files spanning 3 pieces)
const mfTorrent = {
    info: {
        name: Buffer.from('test-multi'),
        'piece length': 512,
        pieces: crypto.randomBytes(60), // 3 pieces
        files: [
            { length: 700, path: [Buffer.from('a.txt')] },
            { length: 836, path: [Buffer.from('b.txt')] },
        ]
    }
};

test('FileManager: single-file torrent creates correct output path', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    assertEqual(fm.files.length, 1, 'should have 1 file');
    assert(fm.files[0].filePath.endsWith('test-single.txt'), 'wrong filename');
    assert(fm.totalSize === 1024, 'wrong total size');
    fm.close();
});

test('FileManager: multi-file torrent creates correct file list', () => {
    const fm = new FileManager(mfTorrent, tmpDir);
    assertEqual(fm.files.length, 2, 'should have 2 files');
    assert(fm.files[0].filePath.endsWith('a.txt'), 'wrong first file');
    assert(fm.files[1].filePath.endsWith('b.txt'), 'wrong second file');
    assertEqual(fm.files[0].offset, 0, 'first file offset should be 0');
    assertEqual(fm.files[1].offset, 700, 'second file offset should be 700');
    assertEqual(fm.totalSize, 1536, 'wrong total size');
    fm.close();
});

test('FileManager: writePiece + readPiece round-trips correctly', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const data = crypto.randomBytes(512);
    fm.writePiece(0, data);
    const read = fm.readPiece(0, 512);
    assert(read !== null, 'readPiece returned null');
    assert(read.equals(data), 'read data does not match written data');
    fm.close();
});

test('FileManager: writePiece handles cross-file boundary correctly', () => {
    const fm = new FileManager(mfTorrent, tmpDir);
    // Piece 1 spans bytes 512-1023: partially in a.txt (512-699) and b.txt (700-1023)
    const pieceData = crypto.randomBytes(512);
    fm.writePiece(1, pieceData);
    const read = fm.readPiece(1, 512);
    assert(read !== null, 'readPiece returned null');
    assert(read.equals(pieceData), 'cross-file piece data mismatch');
    fm.close();
});

test('FileManager: buildBitfield sets correct bits', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const received = new Set([0]);
    const bf = fm.buildBitfield(received, 2);
    // piece 0 = bit 7 of byte 0 = 0x80
    assertEqual(bf[0], 0x80, 'bit for piece 0 should be set');
    fm.close();
});

test('FileManager: buildBitfield with no pieces is all zeros', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const bf = fm.buildBitfield(new Set(), 8);
    assert(bf.every(b => b === 0), 'empty bitfield should be all zeros');
    fm.close();
});

test('FileManager: buildBitfield with all pieces is all 0xFF', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const received = new Set([0, 1, 2, 3, 4, 5, 6, 7]);
    const bf = fm.buildBitfield(received, 8);
    assertEqual(bf[0], 0xFF, 'all pieces set should be 0xFF');
    fm.close();
});

// ══════════════════════════════════════════════════════════════════════════
// 7. Seeder.js
// ══════════════════════════════════════════════════════════════════════════
import Seeder from '../src/Seeder.js';

console.log('\n── Seeder.js ────────────────────────────────────────────────');

test('Seeder: instantiates without error', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const pieces = new Pieces(2);
    const seeder = new Seeder(sfTorrent, fm, pieces, 0);
    assert(seeder !== null);
    fm.close();
});

test('Seeder: start() creates a listening TCP server', (done) => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const pieces = new Pieces(2);
    const seeder = new Seeder(sfTorrent, fm, pieces, 0); // port 0 = OS picks one
    seeder.start();
    assert(seeder.server !== null, 'server should exist');
    assert(seeder.server.listening, 'server should be listening');
    seeder.stop();
    fm.close();
});

test('Seeder: stop() closes the server', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const pieces = new Pieces(2);
    const seeder = new Seeder(sfTorrent, fm, pieces, 0);
    seeder.start();
    seeder.stop();
    assert(!seeder.server || !seeder.server.listening, 'server should be stopped');
    fm.close();
});

test('Seeder: announceHave does not throw with no connections', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const pieces = new Pieces(2);
    const seeder = new Seeder(sfTorrent, fm, pieces, 0);
    // Should not throw even with 0 connections
    seeder.announceHave(0);
    fm.close();
});

test('Seeder: buildBitfield marks received pieces correctly', () => {
    const fm = new FileManager(sfTorrent, tmpDir);
    const pieces = new Pieces(2);
    pieces.addReceived(0);
    const seeder = new Seeder(sfTorrent, fm, pieces, 0);
    const bf = fm.buildBitfield(pieces.received, 2);
    // piece 0 → bit 7 of byte 0
    assertEqual(bf[0] & 0x80, 0x80, 'piece 0 should be set in bitfield');
    // piece 1 → bit 6 of byte 0
    assertEqual(bf[0] & 0x40, 0, 'piece 1 should not be set');
    fm.close();
});

// ══════════════════════════════════════════════════════════════════════════
// 8. Magnet.js
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── Magnet.js ────────────────────────────────────────────────');

import { parseMagnet } from '../src/Magnet.js';

const HEX_40 = 'a'.repeat(40);

function base32Encode(buf) {
    const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
    let bits = 0, value = 0;
    let out = '';
    for (const byte of buf) {
        value = (value << 8) | byte;
        bits += 8;
        while (bits >= 5) {
            out += alphabet[(value >>> (bits - 5)) & 31];
            bits -= 5;
        }
    }
    if (bits > 0) out += alphabet[(value << (5 - bits)) & 31];
    return out;
}

test('parseMagnet: hex info hash, display name, and tracker URLs', () => {
    const m = parseMagnet(
        'magnet:?xt=urn:btih:' + HEX_40 +
        '&dn=hello%20world' +
        '&tr=udp%3A%2F%2Ftracker.example.com%3A6881' +
        '&tr=udp%3A%2F%2Fb.example.org%3A1234'
    );
    assert(m.infoHash.equals(Buffer.from(HEX_40, 'hex')), 'info hash mismatch');
    assertEqual(m.infoHash.length, 20, 'info hash must be 20 bytes');
    assertEqual(m.displayName, 'hello world', 'display name not decoded');
    assertEqual(m.trackers.length, 2, 'should collect two trackers');
    assertEqual(m.trackers[0], 'udp://tracker.example.com:6881', 'first tracker wrong');
});

test('parseMagnet: base32 info hash decodes back to 20 bytes', () => {
    const raw = crypto.randomBytes(20);
    const b32 = base32Encode(raw);
    assertEqual(b32.length, 32, 'padding-free base32 of 20 bytes must be 32 chars');
    const m = parseMagnet('magnet:?xt=urn:btih:' + b32);
    assert(m.infoHash.equals(raw), 'base32 round trip failed');
});

test('parseMagnet: tr parameters may repeat, dn may be absent', () => {
    const m = parseMagnet('magnet:?xt=urn:btih:' + HEX_40 + '&tr=' + encodeURIComponent('http://x/y'));
    assertEqual(m.trackers.length, 1, 'should have one tracker');
    assertEqual(m.displayName, null, 'no dn means null');
});

test('parseMagnet: rejects non-magnet uris', () => {
    let threw = false;
    try { parseMagnet('http://example.com/thing'); } catch (_) { threw = true; }
    assert(threw, 'should throw for non-magnet input');
});

test('parseMagnet: rejects magnet without xt=urn:btih:', () => {
    let threw = false;
    try { parseMagnet('magnet:?dn=nothing'); } catch (_) { threw = true; }
    assert(threw, 'should throw without xt');
});

test('parseMagnet: rejects hash strings of the wrong length', () => {
    let threw = false;
    try { parseMagnet('magnet:?xt=urn:btih:abcd'); } catch (_) { threw = true; }
    assert(threw, 'should throw for a 4-char hash');
});

// ══════════════════════════════════════════════════════════════════════════
// 9. DHT.js — builders/parsers + live localhost integration
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── DHT.js ───────────────────────────────────────────────────');

import DHTNode, {
    buildPingQuery, buildGetPeersQuery, buildAnnouncePeerQuery,
    parseDhtMessage, parseCompactPeers, parseCompactNodes,
    compactNode, compactPeer, xorDistance, isCloser,
} from '../src/DHT.js';

test('buildPingQuery round-trips through the decoder', () => {
    const msg = parseDhtMessage(buildPingQuery(Buffer.alloc(20, 7)));
    assertEqual(msg.y, 'q', 'wrong type');
    assertEqual(msg.q, 'ping', 'wrong query');
    assert(Buffer.isBuffer(msg.a.id), 'id should be a Buffer');
    assertEqual(msg.a.id.length, 20, 'id wrong length');
});

test('buildGetPeersQuery carries the info_hash', () => {
    const ih = crypto.randomBytes(20);
    const msg = parseDhtMessage(buildGetPeersQuery(Buffer.alloc(20), ih));
    assertEqual(msg.q, 'get_peers', 'wrong query');
    assert(msg.a.info_hash.equals(ih), 'info hash mismatch');
});

test('buildAnnouncePeerQuery carries port and token', () => {
    const ih = crypto.randomBytes(20);
    const token = crypto.randomBytes(8);
    const msg = parseDhtMessage(buildAnnouncePeerQuery(Buffer.alloc(20), ih, 6881, token));
    assertEqual(msg.q, 'announce_peer', 'wrong query');
    assertEqual(msg.a.port, 6881, 'wrong port');
    assert(msg.a.token.equals(token), 'token mismatch');
});

test('compactPeer / parseCompactPeers round-trip', () => {
    const peer = { ip: '10.1.2.3', port: 51413 };
    const parsed = parseCompactPeers(compactPeer(peer));
    assertEqual(parsed.length, 1, 'should parse one peer');
    assertEqual(parsed[0].ip, peer.ip, 'ip mismatch');
    assertEqual(parsed[0].port, peer.port, 'port mismatch');
});

test('compactNode / parseCompactNodes round-trip', () => {
    const node = { ip: '192.168.0.9', port: 9001, id: crypto.randomBytes(20) };
    const parsed = parseCompactNodes(compactNode(node));
    assertEqual(parsed.length, 1, 'should parse one node');
    assertEqual(parsed[0].ip, node.ip, 'ip mismatch');
    assertEqual(parsed[0].port, node.port, 'port mismatch');
    assert(parsed[0].id.equals(node.id), 'id mismatch');
});

test('xorDistance + isCloser order nodes by XOR distance', () => {
    const target = Buffer.alloc(20);            // all-zero target
    const closer = Buffer.alloc(20);            // zero → distance zero
    const farther = Buffer.alloc(20);
    farther[19] = 0x01;
    assert(Buffer.isBuffer(xorDistance(closer, target)));
    assert(isCloser(closer, farther, target), 'closer should not be farther');
    assert(!isCloser(farther, closer, target), 'farther should not be closer');
});

await testA('DHT live integration: A announces, C discovers B through A', async () => {
    const infoHash = crypto.randomBytes(20);
    const key = infoHash.toString('hex');

    const nodeA = new DHTNode({ port: 0, bootstrap: [] });
    const nodeB = new DHTNode({ port: 0, bootstrap: [] });
    const nodeC = new DHTNode({ port: 0, bootstrap: [] });

    await nodeA.start();
    await nodeB.start();
    await nodeC.start();

    try {
        // Seed B and C routing tables with A directly (offline bootstrap)
        for (const node of [nodeB, nodeC]) {
            node.table.set(`127.0.0.1:${nodeA.port}`, {
                id: crypto.randomBytes(20), ip: '127.0.0.1', port: nodeA.port,
                token: null, lastSeen: Date.now(),
            });
        }

        // B announces itself for infoHash via A
        const bResult = await nodeB.getPeers(infoHash, {
            timeoutMs: 5000, announcePort: nodeB.port,
        });
        assertEqual(bResult.announced, true, 'B should have announced');
        assertEqual(bResult.peers.length, 0, 'B should find nobody yet');

        const swarm = nodeA.swarms.get(key);
        assert(swarm && swarm.size === 1, 'A should track B in the swarm');

        // C bootstraps to A and discovers B
        const cResult = await nodeC.getPeers(infoHash, {
            timeoutMs: 5000, announcePort: nodeC.port,
        });
        assert(cResult.peers.some(p => p.ip === '127.0.0.1' && p.port === nodeB.port),
            `C should discover B (got ${JSON.stringify(cResult.peers)})`);
        assertEqual(nodeA.swarms.get(key).size, 2, 'A should now track both B and C');
    } finally {
        nodeA.close(); nodeB.close(); nodeC.close();
    }
});

// ══════════════════════════════════════════════════════════════════════════
// 10. MetadataExchange.js — builders plus a fake seeder over TCP
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── MetadataExchange.js ──────────────────────────────────────');

import {
    buildExtendedHandshakeBytes, buildExtendedMessage,
    buildExtendedHandshakeDict, buildMetadataRequest,
    buildMetadataResponse, buildMetadataReject, parseExtendedMessage,
    bencodePrefixEnd, fetchMetadata,
} from '../src/MetadataExchange.js';
import bencode from 'bencode';
import net from 'net';

test('buildExtendedHandshakeBytes sets the BEP 10 reserved bit', () => {
    const hs = buildExtendedHandshakeBytes(crypto.randomBytes(20), crypto.randomBytes(20));
    assertEqual(hs.length, 68, 'handshake must be 68 bytes');
    assertEqual(hs.toString('utf8', 1, 20), 'BitTorrent protocol', 'wrong pstr');
    assert((hs[25] & 0x10) === 0x10, 'extension bit not set in reserved byte 5');
});

test('buildExtendedMessage frames correctly', () => {
    const payload = bencode.encode({ msg_type: 0, piece: 3 });
    const msg = buildExtendedMessage(5, payload);
    assertEqual(msg.readUInt32BE(0), 2 + payload.length, 'wrong length prefix');
    assertEqual(msg.readUInt8(4), 20, 'id should be 20 (extended)');
    assertEqual(msg.readUInt8(5), 5, 'wrong extended id');
});

test('buildExtendedHandshakeDict advertises ut_metadata', () => {
    const msg = buildExtendedHandshakeDict(1, 6881);
    const parsed = parseExtendedMessage(msg);
    assertEqual(parsed.id, 20, 'should be an extended message');
    assertEqual(parsed.extendedId, 0, 'extended handshake id must be 0');
    assertEqual(parsed.dict.m.ut_metadata, 1, 'should advertise ut_metadata id 1');
    assertEqual(parsed.dict.p, 6881, 'should include our port');
});

test('buildMetadataResponse parses back into dict plus raw data', () => {
    const data = crypto.randomBytes(100);
    const msg = buildMetadataResponse(2, 1, 262144, data);
    const parsed = parseExtendedMessage(msg);
    assertEqual(parsed.extendedId, 2, 'extended id mismatch');
    assertEqual(parsed.dict.msg_type, 1, 'msg_type should be DATA');
    assertEqual(parsed.dict.piece, 1, 'piece mismatch');
    assertEqual(parsed.dict.total_size, 262144, 'total_size mismatch');
    assert(parsed.data.equals(data), 'data payload mismatch');
});

test('buildMetadataReject parses as a reject message', () => {
    const parsed = parseExtendedMessage(buildMetadataReject(2, 3));
    assertEqual(parsed.dict.msg_type, 2, 'msg_type should be REJECT');
    assertEqual(parsed.dict.piece, 3, 'piece mismatch');
});

test('bencodePrefixEnd finds value boundaries', () => {
    const enc = Buffer.from(bencode.encode({ msg_type: 0, piece: 5 }));
    assertEqual(bencodePrefixEnd(enc, 0), enc.length, 'dict end wrong');
    assertEqual(bencodePrefixEnd(Buffer.from('5:hello'), 0), 7, 'string end wrong');
    assertEqual(bencodePrefixEnd(Buffer.from('i40e'), 0), 4, 'integer end wrong');
});

await testA('fetchMetadata end-to-end against a fake seeder over TCP', async () => {
    const info = {
        name: Buffer.from('x'),
        length: 4,
        'piece length': 4,
        pieces: crypto.randomBytes(20),
    };
    const encoded = bencode.encode(info);
    const infoHash = crypto.createHash('sha1').update(encoded).digest();
    const metaSize = encoded.length;
    const blockSize = 16384;

    const server = net.createServer((sock) => {
        let buf = Buffer.alloc(0);

        sock.on('data', (data) => {
            buf = Buffer.concat([buf, data]);
            if (!sentHello) {
                // Gate applies only until the client's handshake is seen —
                // after that arbitrary-length frames must flow to drain()
                // (a lone request frame is only ~25 bytes).
                if (buf.length < 68) {
                    return; // still waiting for the client's handshake
                }
                if (buf.toString('utf8', 1, 20) !== 'BitTorrent protocol') return;
                sentHello = true;
                buf = buf.subarray(68);
                sock.write(buildExtendedHandshakeBytes(infoHash, crypto.randomBytes(20)));
                sock.write(buildExtendedMessage(0, bencode.encode({
                    m: { ut_metadata: 2 },
                    p: 6881,
                    v: 'fake-seeder',
                    metadata_size: metaSize,
                })));
            }
            drain();
        });
        let sentHello = false;

        function drain() {
            while (buf.length >= 4) {
                const len = buf.readUInt32BE(0) + 4;
                if (len <= 4 || buf.length < len) return;
                const msg = buf.subarray(0, len);
                buf = buf.subarray(len);
                const parsed = parseExtendedMessage(msg);
                if (parsed.extendedId === 2 && parsed.dict?.msg_type === 0) {
                    const pieceIndex = parsed.dict.piece;
                    const data = encoded.subarray(
                        pieceIndex * blockSize,
                        Math.min((pieceIndex + 1) * blockSize, metaSize)
                    );
                    sock.write(buildMetadataResponse(2, pieceIndex, metaSize, data));
                }
            }
        }
    });

    const port = await new Promise((resolve) =>
        server.listen(0, '127.0.0.1', () => resolve(server.address().port)));

    try {
        // 30s budget: on shared CI runners (three matrix jobs on one VM)
        // this round trip can take a while — the default timeout applies.
        const result = await fetchMetadata({
            infoHash,
            hosts: [{ ip: '127.0.0.1', port }],
            timeoutMs: 30000,
        });
        assert(Buffer.isBuffer(result.name) && result.name.equals(info.name), 'name mismatch');
        assertEqual(result.length, info.length, 'length mismatch');
        assert(result.pieces.equals(info.pieces), 'pieces mismatch');
    } finally {
        server.close();
    }
});

// ══════════════════════════════════════════════════════════════════════════
// 11. server.js — web UI API end-to-end
// ══════════════════════════════════════════════════════════════════════════
console.log('\n── server.js (web UI) ───────────────────────────────────────');

import { spawn } from 'child_process';

async function httpJson(method, pathName, body) {
    const res = await fetch(`http://127.0.0.1:${waitPort}${pathName}`, {
        method,
        headers: body ? { 'Content-Type': 'application/json' } : undefined,
        body: body ? JSON.stringify(body) : undefined,
    });
    return res;
}

let waitPort = null;

await testA('web server: dashboard, add via path, list, and remove — full API round trip', async () => {
    // Synthetic torrent file (single tiny file, junk announce that fails fast)
    const info = {
        name: Buffer.from('webui-test.bin'),
        length: 64,
        'piece length': 64,
        pieces: crypto.randomBytes(20),
    };
    const tmpTorrent = path.join(tmpDir, `fnc-webui-${Date.now()}.torrent`);
    await (await import('fs/promises')).writeFile(tmpTorrent, bencode.encode({
        announce: Buffer.from('udp://127.0.0.1:1'),
        info,
    }));

    const srv = spawn(process.execPath, ['server.js', '--port', '0', '--host', '127.0.0.1'], {
        // Node 18 has no import.meta.dirname — derive the tests dir from the URL
        cwd: path.join(path.dirname(fileURLToPath(import.meta.url)), '..'),
        stdio: ['ignore', 'pipe', 'pipe'],
    });
    srv.stdout.setEncoding('utf8');
    srv.stderr.setEncoding('utf8');
    srv.stdout.on('data', d => process.stdout.write(`    [srv] ${d}`));
    srv.stderr.on('data', d => process.stdout.write(`    [srverr] ${d}`));

    // Wait for the port the server actually bound (it prints it; '0' → random)
    waitPort = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('server did not come up in 10s')), 10000);
        srv.stdout.on('data', (d) => {
            const m = /http:\/\/[^:]+:(\d+)/.exec(d);
            if (m) { clearTimeout(timer); resolve(Number(m[1])); }
        });
    });

    try {
        // Dashboard HTML served
        const home = await httpJson('GET', '/');
        assertEqual(home.status, 200, 'dashboard should be 200');
        assert((await home.text()).includes('FreeTorrent'), 'dashboard HTML wrong');

        // Add the torrent via file path
        const addRes = await httpJson('POST', '/api/add', { torrentPath: tmpTorrent });
        assertEqual(addRes.status, 200, `add should be 200 (got ${addRes.status})`);
        const { id } = await addRes.json();
        assert(id, 'add should return an id');

        // It should appear in the list with its real name within a couple
        // seconds (name arrives once the .torrent file is parsed)
        let listed = null;
        for (let i = 0; i < 20; i++) {
            const list = await (await httpJson('GET', '/api/torrents')).json();
            listed = list.find(t => t.id === id && t.name === 'webui-test.bin');
            if (listed) break;
            await sleep(250);
        }
        assert(listed, 'torrent should appear in /api/torrents');
        assertEqual(listed.name, 'webui-test.bin', 'name should come from the torrent');
        assert(['loading', 'downloading'].includes(listed.status), `status should progress (got ${listed.status})`);

        // Bad add request is rejected
        const badRes = await httpJson('POST', '/api/add', {});
        assertEqual(badRes.status, 400, 'add without torrentPath/magnet must be 400');

        // Remove works
        const remRes = await httpJson('POST', '/api/remove', { id });
        assertEqual(remRes.status, 200, 'remove should be 200');
        const after = await (await httpJson('GET', '/api/torrents')).json();
        assert(!after.some(t => t.id === id), 'torrent should be gone after remove');
    } finally {
        srv.kill('SIGINT');
        await new Promise(r => { srv.on('exit', r); setTimeout(r, 1500); });
    }
});

// ══════════════════════════════════════════════════════════════════════════
// Summary
// ══════════════════════════════════════════════════════════════════════════
console.log(`\n${'─'.repeat(55)}`);
console.log(`  Results: ${passed} passed, ${failed} failed`);
console.log(`${'─'.repeat(55)}\n`);
if (failed > 0) process.exit(1);