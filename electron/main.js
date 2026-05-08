/**
 * electron/main.js
 */

import { app, BrowserWindow, ipcMain, dialog, shell } from 'electron';
import path from 'path';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const activeDownloads = new Map();
let mainWindow = null;

// ── Window ─────────────────────────────────────────────────────────────────
function createWindow() {
    mainWindow = new BrowserWindow({
        width: 960,
        height: 680,
        minWidth: 720,
        minHeight: 500,
        titleBarStyle: 'hiddenInset',
        backgroundColor: '#0a0a0f',
        webPreferences: {
            preload: path.join(__dirname, 'preload.cjs'),
            contextIsolation: true,
            nodeIntegration: false,
            sandbox: false,
        },
        show: false,
    });

    mainWindow.loadFile(path.join(__dirname, '../renderer/index.html'));

    // Open DevTools so we can see renderer errors
    mainWindow.webContents.openDevTools({ mode: 'detach' });

    mainWindow.once('ready-to-show', () => mainWindow.show());
    mainWindow.on('closed', () => { mainWindow = null; });
}

app.whenReady().then(createWindow);

app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') app.quit();
});

app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
});

// ── IPC handlers ───────────────────────────────────────────────────────────
ipcMain.handle('dialog:openTorrent', async () => {
    const result = await dialog.showOpenDialog(mainWindow, {
        title: 'Open Torrent File',
        filters: [{ name: 'Torrent Files', extensions: ['torrent'] }],
        properties: ['openFile', 'multiSelections'],
    });
    return result.canceled ? [] : result.filePaths;
});

ipcMain.handle('dialog:openDirectory', async () => {
    const result = await dialog.showOpenDialog(mainWindow, {
        title: 'Choose Download Location',
        properties: ['openDirectory', 'createDirectory'],
    });
    return result.canceled ? null : result.filePaths[0];
});

ipcMain.handle('shell:showInFolder', (_, filePath) => {
    shell.showItemInFolder(filePath);
});

ipcMain.handle('torrent:add', async (_, { torrentPath, outputDir }) => {
    try {
        const { open } = await import('../src/TorrentParser.js');
        const torrent = await open(torrentPath);

        const torrentName = torrent.info.name.toString('utf8');
        const isMultiFile = !!torrent.info.files;
        const numPieces   = Math.ceil(torrent.info.pieces.length / 20);
        const totalSize   = torrent.info.files
            ? torrent.info.files.reduce((s, f) => s + f.length, 0)
            : torrent.info.length;

        const resolvedOutput = outputDir || app.getPath('downloads');
        const torrentId = Buffer.from(torrentPath).toString('base64');

        const info = {
            id:          torrentId,
            name:        torrentName,
            size:        totalSize,
            numPieces,
            pieceLength: torrent.info['piece length'],
            isMultiFile,
            outputDir:   resolvedOutput,
            files: isMultiFile
                ? torrent.info.files.map(f => ({
                    name: f.path.map(p =>
                        Buffer.isBuffer(p) ? p.toString('utf8') : String(p)
                    ).join('/'),
                    size: f.length,
                }))
                : [{ name: torrentName, size: totalSize }],
            status:     'checking',
            progress:   0,
            downSpeed:  0,
            upSpeed:    0,
            peers:      0,
            seedPeers:  0,
            verified:   0,
            failed:     0,
            eta:        null,
            downloaded: 0,
            uploaded:   0,
            addedAt:    Date.now(),
        };

        mainWindow?.webContents.send('torrent:added', info);

        runDownload(torrentId, torrent, resolvedOutput, info).catch(err => {
            sendUpdate(torrentId, { status: 'error', error: err.message });
        });

        return { success: true, id: torrentId };
    } catch (err) {
        return { success: false, error: err.message };
    }
});

ipcMain.handle('torrent:remove', (_, torrentId) => {
    const dl = activeDownloads.get(torrentId);
    if (dl) {
        dl.cancelled = true;
        dl.seeder?.stop();
        activeDownloads.delete(torrentId);
    }
    mainWindow?.webContents.send('torrent:removed', torrentId);
    return { success: true };
});

// ── Download engine ────────────────────────────────────────────────────────
async function runDownload(torrentId, torrent, outputDir, info) {
    const dl = { cancelled: false, seeder: null };
    activeDownloads.set(torrentId, dl);

    const { default: FileManager } = await import('../src/FileManager.js');
    const { default: Pieces }      = await import('../src/Pieces.js');
    const { default: Seeder }      = await import('../src/Seeder.js');
    const { getPeers }             = await import('../src/tracker.js');
    const {
        buildHandshake, buildInterested,
        buildRequest, parseMessage
    } = await import('../src/Message.js');
    const crypto = await import('crypto');
    const net    = await import('net');

    const numPieces   = info.numPieces;
    const pieceLength = info.pieceLength;
    const totalSize   = info.size;
    const BLOCK_SIZE  = 2 ** 14;
    const MAX_ACTIVE  = 10;
    const CLIENT_PORT = 6881;
    const INVALID_IPS = new Set(['0.0.0.0', '127.0.0.1', '255.255.255.255']);

    const fileManager = new FileManager(torrent, outputDir);
    const pieces = new Pieces(numPieces);

    // Resume
    sendUpdate(torrentId, { status: 'checking', progress: 0 });
    let resumed = 0;
    for (let i = 0; i < numPieces; i++) {
        if (dl.cancelled) { fileManager.close(); return; }
        const buf = fileManager.readPieceForVerify(i, numPieces);
        if (buf) {
            const expected = torrent.info.pieces.slice(i * 20, i * 20 + 20);
            const actual = crypto.default.createHash('sha1').update(buf).digest();
            if (actual.equals(expected)) { pieces.addReceived(i); resumed++; }
        }
        if (i % 100 === 0) {
            sendUpdate(torrentId, {
                status:   'checking',
                progress: Math.round((i / numPieces) * 100),
                verified: resumed,
            });
        }
    }

    if (pieces.isDone()) {
        sendUpdate(torrentId, { status: 'seeding', progress: 100, verified: numPieces });
        const seeder = new Seeder(torrent, fileManager, pieces, CLIENT_PORT);
        dl.seeder = seeder;
        seeder.start();
        return;
    }

    sendUpdate(torrentId, {
        status:   'downloading',
        progress: Math.round((resumed / numPieces) * 100),
        verified: resumed,
    });

    let downloaded = 0, uploaded = 0;
    let lastDown = 0, lastUp = 0, lastTick = Date.now();
    let downBps = 0, upBps = 0;
    let verifiedCount = resumed, failedCount = 0, dlPeers = 0;

    const ticker = setInterval(() => {
        if (dl.cancelled) { clearInterval(ticker); return; }
        const now = Date.now();
        const elapsed = (now - lastTick) / 1000;
        if (elapsed >= 1) {
            downBps  = (downloaded - lastDown) / elapsed;
            upBps    = (uploaded   - lastUp)   / elapsed;
            lastDown = downloaded;
            lastUp   = uploaded;
            lastTick = now;
        }
        const remaining = (numPieces - verifiedCount) * pieceLength;
        sendUpdate(torrentId, {
            status:    'downloading',
            progress:  Math.round((verifiedCount / numPieces) * 100),
            downSpeed: downBps,
            upSpeed:   upBps,
            peers:     dlPeers,
            seedPeers: dl.seeder?.connections.size ?? 0,
            verified:  verifiedCount,
            failed:    failedCount,
            eta:       downBps > 0 ? Math.floor(remaining / downBps) : null,
            downloaded,
            uploaded,
        });
    }, 1000);

    const seeder = new Seeder(torrent, fileManager, pieces, CLIENT_PORT);
    dl.seeder = seeder;
    seeder.onUpload = bytes => { uploaded += bytes; };
    seeder.start();

    while (!pieces.isDone() && !dl.cancelled) {
        const peers = await new Promise(resolve =>
            getPeers(torrent, CLIENT_PORT, p => resolve(p))
        );

        const valid = peers.filter(p =>
            p && typeof p.port === 'number' &&
            p.port > 0 && p.port < 65536 &&
            !INVALID_IPS.has(p.ip)
        );

        if (!valid.length) { await sleep(15000); continue; }

        await new Promise(resolve => {
            let idx = 0, active = 0;

            function spawnNext() {
                if (pieces.isDone() || dl.cancelled) {
                    if (active === 0) resolve();
                    return;
                }
                while (active < MAX_ACTIVE && idx < valid.length) {
                    connectPeer(valid[idx++]);
                    active++;
                    dlPeers = active;
                }
                if (idx >= valid.length && active === 0) resolve();
            }

            function done() {
                active--;
                dlPeers = active;
                if (pieces.isDone() || dl.cancelled) {
                    if (active === 0) resolve();
                } else {
                    spawnNext();
                    if (idx >= valid.length && active === 0) resolve();
                }
            }

            function connectPeer(peer) {
                const socket = new net.default.Socket();
                let closed = false;

                function close() {
                    if (closed) return;
                    closed = true;
                    try { socket.destroy(); } catch (_) {}
                    done();
                }

                socket.setTimeout(15000);
                socket.on('timeout', close);
                socket.on('error',   close);
                socket.on('close',   close);

                socket.connect(peer.port, peer.ip, () => {
                    socket.write(buildHandshake(torrent));
                    handleMessages(socket, close);
                });
            }

            function handleMessages(socket, close) {
                let buf = Buffer.alloc(0);
                let handshake = true;
                const queue = { choked: true, queue: [] };
                const pieceBufs = {};

                socket.on('data', data => {
                    buf = Buffer.concat([buf, data]);
                    drain();
                });

                function drain() {
                    try {
                        if (!buf.length) return;
                        const len = handshake
                            ? buf[0] + 49
                            : buf.readUInt32BE(0) + 4;
                        if (len <= 0 || len > 131072) { buf = Buffer.alloc(0); return; }
                        if (buf.length < len) return;
                        const msg = buf.subarray(0, len);
                        buf = buf.subarray(len);
                        handshake = false;
                        handle(msg);
                        drain();
                    } catch { close(); }
                }

                function handle(msg) {
                    if (msg.length >= 20 &&
                        msg.length === msg.readUInt8(0) + 49 &&
                        msg.toString('utf8', 1, 20) === 'BitTorrent protocol') {
                        socket.write(buildInterested());
                        return;
                    }
                    if (msg.length < 4) return;
                    const { id, payload } = parseMessage(msg);
                    if      (id === 0) { queue.choked = true; close(); }
                    else if (id === 1) { queue.choked = false; requestPiece(); }
                    else if (id === 4) { onHave(payload); }
                    else if (id === 5) { onBitfield(payload); }
                    else if (id === 7) { onPiece(payload); }
                }

                function onHave(payload) {
                    if (!payload || payload.length < 4) return;
                    const pi = payload.readUInt32BE(0);
                    if (pi >= 0 && pi < numPieces) {
                        queue.queue.push(pi);
                        if (queue.queue.length === 1) requestPiece();
                    }
                }

                function onBitfield(payload) {
                    if (!payload) return;
                    for (let i = 0; i < payload.length; i++)
                        for (let j = 0; j < 8; j++) {
                            const pi = i * 8 + j;
                            if (pi < numPieces && (payload[i] >> (7 - j)) & 1)
                                queue.queue.push(pi);
                        }
                    requestPiece();
                }

                function onPiece(payload) {
                    if (!payload?.block) return;
                    const pi     = payload.index;
                    const isLast = pi === numPieces - 1;
                    const tpl    = isLast
                        ? totalSize % pieceLength || pieceLength
                        : pieceLength;

                    if (!pieceBufs[pi]) pieceBufs[pi] = Buffer.alloc(tpl);
                    payload.block.copy(pieceBufs[pi], payload.begin);
                    if (payload.begin + payload.block.length < tpl) return;

                    const pieceBuf = pieceBufs[pi];
                    delete pieceBufs[pi];

                    const expected = torrent.info.pieces.slice(pi * 20, pi * 20 + 20);
                    const actual   = crypto.default
                        .createHash('sha1').update(pieceBuf).digest();

                    if (actual.equals(expected)) {
                        try {
                            fileManager.writePiece(pi, pieceBuf);
                            pieces.addReceived(pi);
                            verifiedCount++;
                            downloaded += pieceBuf.length;
                            seeder.announceHave(pi);
                            queue.queue.shift();
                            requestPiece();
                            if (pieces.isDone()) close();
                        } catch { close(); }
                    } else {
                        pieces.requested.delete(pi);
                        queue.queue.push(pi);
                        failedCount++;
                        requestPiece();
                    }
                }

                function requestPiece() {
                    if (queue.choked) return;
                    while (queue.queue.length) {
                        const pi = queue.queue[0];
                        if (pieces.needed(pi)) {
                            const isLast = pi === numPieces - 1;
                            const tpl    = isLast
                                ? totalSize % pieceLength || pieceLength
                                : pieceLength;
                            for (let begin = 0; begin < tpl; begin += BLOCK_SIZE) {
                                try {
                                    socket.write(buildRequest({
                                        index:  pi,
                                        begin,
                                        length: Math.min(BLOCK_SIZE, tpl - begin),
                                    }));
                                } catch (_) { return; }
                            }
                            pieces.addRequested(pi);
                            queue.queue.shift();
                            break;
                        }
                        queue.queue.shift();
                    }
                }
            }

            spawnNext();
        });

        if (!pieces.isDone() && !dl.cancelled) await sleep(10000);
    }

    clearInterval(ticker);

    if (!dl.cancelled) {
        fileManager.close();
        sendUpdate(torrentId, {
            status:    'seeding',
            progress:  100,
            verified:  numPieces,
            downSpeed: 0,
            peers:     0,
        });
    }
}

function sendUpdate(torrentId, data) {
    mainWindow?.webContents.send('torrent:update', { id: torrentId, ...data });
}

function sleep(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
}