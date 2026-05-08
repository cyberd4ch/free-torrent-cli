/**
 * renderer/app.js
 * Frontend logic. All engine calls go through window.torrentAPI (preload bridge).
 */

// ── State ──────────────────────────────────────────────────────────────────
const state = {
    torrents: new Map(),
    selectedId: null,
    savePath: null,
};

// ── DOM refs ───────────────────────────────────────────────────────────────
const emptyState = document.getElementById('emptyState');
const torrentList = document.getElementById('torrentList');
const statActive = document.getElementById('statActive');
const statSeeding = document.getElementById('statSeeding');
const statDown = document.getElementById('statDown');
const statUp = document.getElementById('statUp');
const savePathDisplay = document.getElementById('savePathDisplay');
const dropOverlay = document.getElementById('dropOverlay');
const toast = document.getElementById('toast');

// ── Init ───────────────────────────────────────────────────────────────────
function init() {
    document.getElementById('btnAdd').addEventListener('click', openTorrentDialog);
    document.getElementById('btnAddEmpty').addEventListener('click', openTorrentDialog);
    document.getElementById('btnChangeSavePath').addEventListener('click', changeSavePath);
    document.getElementById('btnChangeSavePath2').addEventListener('click', changeSavePath);

    // Drag and drop — Electron exposes file.path on File objects in the renderer
    document.addEventListener('dragenter', e => {
        e.preventDefault();
        if ([...e.dataTransfer.items].some(i => i.kind === 'file')) {
            dropOverlay.classList.add('active');
        }
    });
    document.addEventListener('dragover', e => e.preventDefault());
    document.addEventListener('dragleave', e => {
        if (!e.relatedTarget) dropOverlay.classList.remove('active');
    });
    document.addEventListener('drop', e => {
        e.preventDefault();
        dropOverlay.classList.remove('active');
        const files = [...e.dataTransfer.files];
        files
            .filter(f => f.name.endsWith('.torrent'))
            .forEach(f => {
                // In Electron (non-sandboxed renderer), File objects have a .path property
                const filePath = f.path || f.name;
                if (filePath) addTorrent(filePath);
            });
    });

    // IPC events from main process
    window.torrentAPI.onTorrentAdded(onTorrentAdded);
    window.torrentAPI.onTorrentUpdate(onTorrentUpdate);
    window.torrentAPI.onTorrentRemoved(onTorrentRemoved);
}

// ── Dialogs ────────────────────────────────────────────────────────────────
async function openTorrentDialog() {
    const paths = await window.torrentAPI.openTorrentDialog();
    if (!paths || paths.length === 0) return;
    paths.forEach(p => addTorrent(p));
}

async function changeSavePath() {
    const dir = await window.torrentAPI.openDirectoryDialog();
    if (!dir) return;
    state.savePath = dir;
    const display = dir.replace(/\/Users\/[^/]+/, '~');
    savePathDisplay.textContent = display;
    showToast('Save location updated', 'success');
}

async function addTorrent(torrentPath) {
    const result = await window.torrentAPI.addTorrent(torrentPath, state.savePath);
    if (!result.success) showToast(`Error: ${result.error}`, 'error');
}

// ── IPC handlers ───────────────────────────────────────────────────────────
function onTorrentAdded(info) {
    state.torrents.set(info.id, info);
    renderCard(info);
    updateListVisibility();
    updateSidebar();
    showToast(`Added: ${truncate(info.name, 40)}`, 'success');
}

function onTorrentUpdate(data) {
    const torrent = state.torrents.get(data.id);
    if (!torrent) return;
    Object.assign(torrent, data);
    updateCard(torrent);
    updateSidebar();
}

function onTorrentRemoved(id) {
    state.torrents.delete(id);
    const card = document.getElementById(`card-${CSS.escape(id)}`);
    if (card) {
        card.style.opacity = '0';
        card.style.transform = 'translateX(-8px)';
        card.style.transition = 'all 0.2s ease';
        setTimeout(() => card.remove(), 200);
    }
    if (state.selectedId === id) state.selectedId = null;
    updateListVisibility();
    updateSidebar();
}

// ── Card rendering ─────────────────────────────────────────────────────────
function renderCard(torrent) {
    const card = document.createElement('div');
    card.className = 'torrent-card';
    card.id = `card-${torrent.id}`;
    card.innerHTML = cardHTML(torrent);
    card.style.opacity = '0';
    card.style.transform = 'translateY(6px)';

    torrentList.appendChild(card);

    requestAnimationFrame(() => {
        card.style.transition = 'all 0.2s ease';
        card.style.opacity = '1';
        card.style.transform = 'translateY(0)';
    });

    attachCardListeners(card, torrent);
}

function updateCard(torrent) {
    const card = document.getElementById(`card-${torrent.id}`);
    if (!card) return;

    const wasSelected = state.selectedId === torrent.id;
    card.innerHTML = cardHTML(torrent);
    if (wasSelected) card.classList.add('selected');
    attachCardListeners(card, torrent);

    // Restore detail panel if this card was expanded
    if (wasSelected) {
        const detail = card.querySelector('.detail-panel');
        if (detail) detail.classList.add('visible');
    }
}

function attachCardListeners(card, torrent) {
    card.addEventListener('click', e => {
        if (e.target.closest('.icon-btn')) return;
        toggleDetail(torrent.id);
    });

    card.querySelector('.btn-remove')?.addEventListener('click', async e => {
        e.stopPropagation();
        await window.torrentAPI.removeTorrent(torrent.id);
    });

    card.querySelector('.btn-folder')?.addEventListener('click', async e => {
        e.stopPropagation();
        const p = torrent.outputDir
            ? `${torrent.outputDir}/${torrent.name}`
            : torrent.name;
        await window.torrentAPI.showInFolder(p);
    });
}

function cardHTML(t) {
    const pct = t.progress || 0;
    const fillClass = t.status === 'seeding' ? 'seeding' : t.status === 'error' ? 'error' : '';

    const statusLabel = {
        checking: 'Checking',
        downloading: 'Downloading',
        seeding: 'Seeding',
        error: 'Error',
    }[t.status] || t.status;

    return `
    <div class="card-header">
      <div class="card-name">${escapeHTML(t.name)}</div>
      <div class="card-actions">
        <button class="icon-btn btn-folder" title="Show in Finder">
          <svg viewBox="0 0 16 16"><path d="M1 3.5A1.5 1.5 0 012.5 2h3.764c.958 0 1.76.56 2.09 1.328L8.5 4H13.5A1.5 1.5 0 0115 5.5v7A1.5 1.5 0 0113.5 14h-11A1.5 1.5 0 011 12.5v-9z"/></svg>
        </button>
        <button class="icon-btn btn-remove danger" title="Remove">
          <svg viewBox="0 0 16 16"><path d="M6.5 1h3a.5.5 0 01.5.5v1H6v-1a.5.5 0 01.5-.5zM11 2.5v-1A1.5 1.5 0 009.5 0h-3A1.5 1.5 0 005 1.5v1H2.506a.58.58 0 00-.01 1.16l.337 9.136A1.5 1.5 0 004.33 14h7.34a1.5 1.5 0 001.498-1.204l.337-9.136a.58.58 0 00-.01-1.16H11z"/></svg>
        </button>
      </div>
    </div>

    <div class="card-progress-wrap">
      <div class="card-progress-track">
        <div class="card-progress-fill ${fillClass}" style="width:${pct}%"></div>
      </div>
    </div>

    <div class="card-meta">
      <span class="status-badge ${t.status}">
        <span class="status-dot"></span>${statusLabel}
      </span>
      <span class="meta-item">${pct}%</span>
      <span class="meta-item">${formatBytes(t.size)}</span>
      ${buildMetaItems(t)}
    </div>

    <div class="detail-panel" id="detail-${t.id}">
      ${buildFilesHTML(t)}
    </div>`;
}

function buildMetaItems(t) {
    if (t.status === 'checking')
        return `<span class="meta-item">Verifying pieces...</span>`;

    if (t.status === 'downloading') return `
      <span class="meta-item down">↓ ${formatSpeed(t.downSpeed)}</span>
      <span class="meta-item up">↑ ${formatSpeed(t.upSpeed)}</span>
      ${t.eta ? `<span class="meta-item">ETA ${formatEta(t.eta)}</span>` : ''}
      <span class="meta-item">Peers: ${t.peers || 0}</span>
      <span class="meta-item">✓ ${t.verified || 0}</span>
      ${t.failed ? `<span class="meta-item warn">✗ ${t.failed}</span>` : ''}`;

    if (t.status === 'seeding') return `
      <span class="meta-item up">↑ ${formatSpeed(t.upSpeed)}</span>
      <span class="meta-item">Peers: ${t.seedPeers || 0}</span>
      <span class="meta-item">Uploaded: ${formatBytes(t.uploaded || 0)}</span>`;

    if (t.status === 'error')
        return `<span class="meta-item warn">${escapeHTML(t.error || 'Unknown error')}</span>`;

    return '';
}

function buildFilesHTML(t) {
    if (!t.files || t.files.length <= 1) return '';
    const rows = t.files.map(f => `
      <div class="detail-file">
        <span class="detail-file-name">${escapeHTML(f.name)}</span>
        <span class="detail-file-size">${formatBytes(f.size)}</span>
      </div>`).join('');
    return `<div class="detail-files-label">Files (${t.files.length})</div>${rows}`;
}

// ── Detail expand/collapse ─────────────────────────────────────────────────
function toggleDetail(id) {
    const torrent = state.torrents.get(id);
    if (!torrent?.files || torrent.files.length <= 1) return;

    const card = document.getElementById(`card-${id}`);
    const detail = document.getElementById(`detail-${id}`);
    if (!card || !detail) return;

    const isOpen = detail.classList.contains('visible');

    // Close prev
    if (state.selectedId && state.selectedId !== id) {
        const prev = document.getElementById(`card-${state.selectedId}`);
        const prevDetail = document.getElementById(`detail-${state.selectedId}`);
        prev?.classList.remove('selected');
        prevDetail?.classList.remove('visible');
    }

    if (isOpen) {
        detail.classList.remove('visible');
        card.classList.remove('selected');
        state.selectedId = null;
    } else {
        detail.classList.add('visible');
        card.classList.add('selected');
        state.selectedId = id;
    }
}

// ── Sidebar ────────────────────────────────────────────────────────────────
function updateSidebar() {
    let active = 0, seeding = 0, totalDown = 0, totalUp = 0;
    for (const t of state.torrents.values()) {
        if (t.status === 'downloading') { active++; totalDown += t.downSpeed || 0; totalUp += t.upSpeed || 0; }
        if (t.status === 'seeding') { seeding++; totalUp += t.upSpeed || 0; }
    }
    statActive.textContent = active;
    statSeeding.textContent = seeding;
    statDown.textContent = formatSpeed(totalDown);
    statUp.textContent = formatSpeed(totalUp);
}

// ── List visibility ────────────────────────────────────────────────────────
function updateListVisibility() {
    const has = state.torrents.size > 0;
    emptyState.style.display = has ? 'none' : 'flex';
    torrentList.style.display = has ? 'flex' : 'none';
}

// ── Toast ──────────────────────────────────────────────────────────────────
let toastTimer = null;
function showToast(msg, type = '') {
    toast.textContent = msg;
    toast.className = `toast ${type} show`;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => toast.classList.remove('show'), 3000);
}

// ── Formatters ─────────────────────────────────────────────────────────────
function formatBytes(b) {
    if (!b) return '0 B';
    if (b >= 1e9) return `${(b / 1e9).toFixed(2)} GB`;
    if (b >= 1e6) return `${(b / 1e6).toFixed(2)} MB`;
    if (b >= 1e3) return `${(b / 1e3).toFixed(2)} KB`;
    return `${b} B`;
}

function formatSpeed(bps) {
    if (!bps) return '0 B/s';
    if (bps >= 1e6) return `${(bps / 1e6).toFixed(1)} MB/s`;
    if (bps >= 1e3) return `${(bps / 1e3).toFixed(1)} KB/s`;
    return `${Math.round(bps)} B/s`;
}

function formatEta(secs) {
    if (!secs || secs <= 0) return '--:--';
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    if (m >= 60) return `${Math.floor(m / 60)}h ${m % 60}m`;
    return `${m}m ${String(s).padStart(2, '0')}s`;
}

function truncate(str, len) {
    return str.length > len ? str.slice(0, len) + '…' : str;
}

function escapeHTML(str) {
    return String(str)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;')
        .replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// ── Boot ───────────────────────────────────────────────────────────────────
init();