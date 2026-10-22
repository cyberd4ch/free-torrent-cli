/**
 * src/Magnet.js — Parse magnet URIs (BEP 9 form).
 *
 * Supported formats for the `xt` parameter:
 *   magnet:?xt=urn:btih:<40-char hex>
 *   magnet:?xt=urn:btih:<32-char base32>   (decoded back to 20 bytes)
 *
 * `tr` parameters may repeat; `dn` is the display name (URL-encoded).
 */

import { Buffer } from 'buffer';
import DHTNode from './DHT.js';
import { fetchMetadata } from './MetadataExchange.js';

const HEX_RE = /^[0-9a-fA-F]{40}$/;

function decodeBase32(input) {
    const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
    const clean = input.toUpperCase().replace(/[^A-Z2-7]/g, '');
    let bits = 0;
    let value = 0;
    const out = [];

    for (const ch of clean) {
        const idx = alphabet.indexOf(ch);
        if (idx === -1) throw new Error(`invalid base32 character: ${ch}`);
        value = (value << 5) | idx;
        bits += 5;
        if (bits >= 8) {
            out.push((value >>> (bits - 8)) & 0xff);
            bits -= 8;
        }
    }
    return Buffer.from(out);
}

export function parseMagnet(uri) {
    if (typeof uri !== 'string' || !uri.startsWith('magnet:?')) {
        throw new Error('not a magnet URI (must start with "magnet:?")');
    }

    const params = new URLSearchParams(uri.slice('magnet:?'.length));
    const xt = params.getAll('xt').find(v => v.startsWith('urn:btih:'));
    if (!xt) throw new Error('magnet missing "xt=urn:btih:" parameter');

    const hashStr = xt.slice('urn:btih:'.length);

    let infoHash;
    if (HEX_RE.test(hashStr)) {
        infoHash = Buffer.from(hashStr.toLowerCase(), 'hex');
    } else if (hashStr.length === 32) {
        infoHash = decodeBase32(hashStr);
        if (infoHash.length !== 20) {
            throw new Error(`base32 info hash decoded to ${infoHash.length} bytes`);
        }
    } else {
        throw new Error(`unsupported info hash format: "${hashStr}"`);
    }

    const trackers = params.getAll('tr')
        .filter(Boolean)
        // magent URIs are URL-encoded per component
        .map(t => decodeURIComponent(t));

    let displayName = params.get('dn') ?? null;
    if (displayName) {
        try { displayName = decodeURIComponent(displayName); } catch (_) { /* keep raw */ }
    }

    return { infoHash, trackers, displayName };
}

/**
 * Full magnet → torrent resolution pipeline:
 *   1. parse the URI
 *   2. find peers via DHT (BEP 5)
 *   3. fetch the info dict from peers (BEP 9/10), verify the hash
 *   4. build a torrent object compatible with the rest of this codebase
 *      (tracker URLs from the magnet are preserved for the announce loop)
 */
export async function resolveMagnet(uri, {
    dhtPort = 0,              // 0 = ephemeral, good for tests / multiple instances
    timeoutMs = 60000,
    workshopPort = 6881,      // the port peers should reach our seeder on
    onStatus = () => {},
    onMetadataProgress = () => {},
} = {}) {
    onStatus('Parsing magnet URI...');
    const { infoHash, trackers, displayName } = parseMagnet(uri);
    onStatus(`Info hash: ${infoHash.toString('hex')}`);
    if (displayName) onStatus(`Name (from magnet): ${displayName}`);

    onStatus('Starting DHT node for peer discovery...');
    const dht = new DHTNode({ port: dhtPort, log: onStatus });
    const localPort = await dht.start();
    onStatus(`DHT listening on UDP ${localPort}, bootstrapping...`);
    await dht.bootstrap();
    onStatus(`Routing table holds ${dht.table.size} node(s) after bootstrap`);

    let info = null;
    try {
        onStatus('Running iterative get_peers lookup...');
        const { peers } = await dht.getPeers(infoHash, {
            timeoutMs: Math.min(timeoutMs, 15000),
            announcePort: workshopPort,
        });
        onStatus(`DHT found ${peers.length} peer(s)`);

        onStatus('Requesting metadata over the extension protocol (BEP 9/10)...');
        info = await fetchMetadata({
            infoHash,
            hosts: peers,
            timeoutMs,
            onProgress: onMetadataProgress,
        });
    } finally {
        dht.close();
    }

    const torrent = { info };
    if (trackers.length) {
        torrent.announce = trackers[0];
        if (trackers.length > 1) {
            torrent['announce-list'] = trackers.map(t => [t]);
        }
    }
    onStatus(`Metadata verified ✓ (${torrent.info.name?.toString?.('utf8') ?? '?'})`);
    return torrent;
}

export default { parseMagnet, resolveMagnet };
