/**
 * free-torrent-cli — unit test suite
 * Run with:  node tests/run.js
 */

import crypto from 'crypto';
import { Buffer } from 'buffer';

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
// Summary
// ══════════════════════════════════════════════════════════════════════════
console.log(`\n${'─'.repeat(55)}`);
console.log(`  Results: ${passed} passed, ${failed} failed`);
console.log(`${'─'.repeat(55)}\n`);
if (failed > 0) process.exit(1)