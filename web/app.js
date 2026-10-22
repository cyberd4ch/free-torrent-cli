/* web/app.js — dashboard logic. Connects to the SSE stream and renders. */

const listEl = document.getElementById('list');
const dotEl = document.getElementById('connDot');
const logEl = document.getElementById('log');
const addForm = document.getElementById('addForm');

/** id → rendered row element + known status */
const rows = new Map();

function fmtBytes(n) {
    if (n == null) return '—';
    if (n >= 1e9) return `${(n / 1e9).toFixed(2)} GB`;
    if (n >= 1e6) return `${(n / 1e6).toFixed(2)} MB`;
    if (n >= 1e3) return `${(n / 1e3).toFixed(1)} KB`;
    return `${n} B`;
}

function fmtSpeed(bps) {
    if (!bps || bps <= 0) return '0 KB/s';
    return fmtBytes(bps) + '/s';
}

function fmtEta(s) {
    if (s == null) return '—';
    if (s > 3600) return `${Math.floor(s / 3600)}h ${Math.round((s % 3600) / 60)}m`;
    if (s > 60) return `${Math.floor(s / 60)}m ${s % 60}s`;
    return `${s}s`;
}

function row(t) {
    let el = rows.get(t.id);
    if (!el) {
        el = document.createElement('div');
        el.className = 'card torrent';
        el.innerHTML = `
            <div class="t-row">
                <div>
                    <div class="t-name"></div>
                    <div class="t-meta"></div>
                </div>
                <button class="t-remove" title="Stop & remove">✕</button>
            </div>
            <div class="bar"><div></div></div>
            <div class="t-stats"></div>
            <div class="t-error" hidden></div>`;
        el.querySelector('.t-remove').addEventListener('click', async () => {
            await fetch('/api/remove', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ id: t.id }),
            });
        });
        rows.set(t.id, el);

        const empty = listEl.querySelector('.empty');
        if (empty) empty.remove();
        listEl.appendChild(el);
    }
    return el;
}

function render(t) {
    const el = row(t);
    const status = t.status ?? 'unknown';

    el.className = `card torrent status-${status}`;
    el.querySelector('.t-name').textContent = t.name ?? t.id;
    el.querySelector('.t-meta').textContent =
        (t.size != null ? `${fmtBytes(t.size)} · ${t.numPieces ?? '?'} pieces · ` : '') +
        (t.resolving ? 'resolving magnet (DHT + peers)…' : status);
    el.querySelector('.bar > div').style.width = `${Math.min(100, t.progress ?? 0)}%`;

    const stats = {
        '↓': fmtSpeed(t.downSpeed),
        '↑': fmtSpeed(t.upSpeed),
        peers: `${t.peers ?? 0}/${t.seedPeers ?? 0}`,
        eta: t.status === 'downloading' ? fmtEta(t.eta) : '—',
        verified: t.verified ?? '—',
        ok: status,
    };
    el.querySelector('.t-stats').innerHTML = Object.entries(stats)
        .map(([k, v]) => `<span>${k === 'verified' || k === 'ok' ? '' : `<b>${k}</b> `}${v}</span>`)
        .join('');

    const errEl = el.querySelector('.t-error');
    if (status === 'error' && t.error) {
        errEl.hidden = false;
        errEl.textContent = t.error;
    } else {
        errEl.hidden = true;
    }
}

function fullRender(torrentsList) {
    const seen = new Set();
    for (const t of torrentsList) {
        seen.add(t.id);
        render(t);
    }
    for (const [id, el] of rows) {
        if (!seen.has(id)) {
            el.remove();
            rows.delete(id);
        }
    }
    if (!rows.size && !listEl.querySelector('.empty')) {
        listEl.innerHTML = '<div class="empty">No torrents yet — add one above.</div>';
    }
}

addForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const value = document.getElementById('inpTorrent').value.trim();
    const outDir = document.getElementById('inpDir').value.trim();
    if (!value) return;

    const body = outDir
        ? { outputDir: outDir, ...(value.startsWith('magnet:?') ? { magnet: value } : { torrentPath: value }) }
        : (value.startsWith('magnet:?') ? { magnet: value } : { torrentPath: value });

    const res = await fetch('/api/add', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    if (!res.ok) {
        const err = await res.json().catch(() => ({}));
        alert(err.error ?? 'failed to add');
    }
    document.getElementById('inpTorrent').value = '';
});

function connect() {
    const es = new EventSource('/api/events');

    es.onerror = () => {
        dotEl.classList.remove('live');
        // EventSource auto-reconnects; refresh list once reconnected
    };
    es.onopen = () => dotEl.classList.add('live');

    es.addEventListener('update', (e) => {
        const data = JSON.parse(e.data);
        render(data);
    });
    es.addEventListener('added', () => { /* row is created on update */ });
    es.addEventListener('removed', (e) => {
        const { id } = JSON.parse(e.data);
        rows.get(id)?.remove();
        rows.delete(id);
        if (!rows.size) {
            listEl.innerHTML = '<div class="empty">No torrents yet — add one above.</div>';
        }
    });
    es.addEventListener('log', (e) => {
        const { msg } = JSON.parse(e.data);
        logEl.textContent = (logEl.textContent + msg + '\n').split('\n').slice(-200).join('\n');
        logEl.scrollTop = logEl.scrollHeight;
    });

    // Initial full state via one SSE snapshot message
    es.onmessage = (e) => {
        try {
            const data = JSON.parse(e.data);
            if (data.type === 'snapshot') fullRender(data.torrents);
        } catch (_) {}
    };
}

fetch('/api/torrents').then(r => r.json()).then(fullRender).catch(() => {});
connect();
