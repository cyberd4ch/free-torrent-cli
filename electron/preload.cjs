/**
 * electron/preload.js
 * Must use CommonJS require() — preload scripts run in a special
 * Electron context where ESM imports are not supported.
 */

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('torrentAPI', {
    // Dialogs
    openTorrentDialog:   () => ipcRenderer.invoke('dialog:openTorrent'),
    openDirectoryDialog: () => ipcRenderer.invoke('dialog:openDirectory'),
    showInFolder: (filePath) => ipcRenderer.invoke('shell:showInFolder', filePath),

    // Torrent control
    addTorrent:    (torrentPath, outputDir) => ipcRenderer.invoke('torrent:add', { torrentPath, outputDir }),
    removeTorrent: (torrentId) => ipcRenderer.invoke('torrent:remove', torrentId),

    // Events from main → renderer
    onTorrentAdded:   (cb) => ipcRenderer.on('torrent:added',   (_, data) => cb(data)),
    onTorrentUpdate:  (cb) => ipcRenderer.on('torrent:update',  (_, data) => cb(data)),
    onTorrentRemoved: (cb) => ipcRenderer.on('torrent:removed', (_, id)   => cb(id)),
});