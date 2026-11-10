// Codenotch na barra de status do VS Code.
// Sistema (CPU/RAM/SSD): números de `codenotch.exe sysmon` (mesma conta do notch).
// VRAM: contadores do Windows (typeperf). IAs: JSON que o Codenotch grava em %APPDATA%\codenotch.
const vscode = require('vscode');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn, execFile } = require('child_process');

const DIR = path.join(process.env.APPDATA || '', 'codenotch');
// Ferramentas do Windows sempre pelo caminho absoluto (não pelo PATH/pasta atual): evita rodar um .exe plantado.
const sys32 = (exe) => path.join(process.env.SystemRoot || 'C:\Windows', 'System32', exe);
const NOMES = { claude: 'Claude', codex: 'GPT', antigravity: 'Antigravity', cursor: 'Cursor', grok: 'Grok' };
const ARQUIVO = { claude: 'usage.json' };

const VERDE = '#4ade80';
const AMBAR = '#fbbf24';

const itens = new Map(); // chave -> StatusBarItem
const suave = { cpu: null, busy: null };
let sys = null;
let vram = null;
let proc = null;
let parando = false;
let tentativas = 0;
const timers = [];

const lerJson = (arq) => {
  try { return JSON.parse(fs.readFileSync(path.join(DIR, arq), 'utf8')); } catch { return null; }
};
const cfg = () => vscode.workspace.getConfiguration('codenotch');

function exePath() {
  // Só vale o que o dono pôs nas configurações de USUÁRIO: um .vscode/settings.json de projeto
  // baixado não pode escolher qual .exe esta extensão executa.
  const i = cfg().inspect('exe');
  const manual = i && i.globalValue;
  if (manual && path.isAbsolute(manual) && !/^\\\\/.test(manual) && /codenotch\.exe$/i.test(manual)) return manual;
  return path.join(process.env.LOCALAPPDATA || '', 'Programs', 'Codenotch', 'codenotch.exe');
}

// ---------- item da barra ----------
function item(chave, prio) {
  let it = itens.get(chave);
  if (!it) {
    it = vscode.window.createStatusBarItem(`codenotch.${chave}`, vscode.StatusBarAlignment.Right, prio);
    it.name = `Codenotch ${chave}`;
    it.command = 'codenotch.atualizar';
    itens.set(chave, it);
  }
  return it;
}

// Cor pelo nível: verde < 60, âmbar 60–79, fundo amarelo ≥ amarelo, fundo vermelho ≥ vermelho.
function pinta(it, pct) {
  const amarelo = cfg().get('alertaAmarelo');
  const vermelho = cfg().get('alertaVermelho');
  if (pct >= vermelho) { it.backgroundColor = new vscode.ThemeColor('statusBarItem.errorBackground'); it.color = undefined; }
  else if (pct >= amarelo) { it.backgroundColor = new vscode.ThemeColor('statusBarItem.warningBackground'); it.color = undefined; }
  else { it.backgroundColor = undefined; it.color = pct >= 60 ? AMBAR : VERDE; }
}

const med = (rotulo, pct) => `${rotulo} ${Math.round(pct)}%`;

// ---------- sistema: CPU / RAM / SSD ----------
function parseLinha(l) {
  const cpu = /CPU (\d+)%/.exec(l);
  const ram = /RAM ([\d.]+) GB \/ ([\d.]+) GB \((\d+)%\)/.exec(l);
  const disco = /(\w:) ([\d.]+) GB \/ ([\d.]+) GB \((\d+)%\) busy (\d+)%/.exec(l);
  const rede = /down ([\d.]+) Mb\/s up ([\d.]+) Mb\/s/.exec(l);
  if (!cpu || !ram) return null;
  return {
    cpu: +cpu[1],
    ram: +ram[3], ramUsada: ram[1], ramTotal: ram[2],
    disco: disco ? { letra: disco[1], usado: disco[2], total: disco[3], pct: +disco[4], busy: +disco[5] } : null,
    rede: rede ? { down: +rede[1], up: +rede[2] } : null,
  };
}

// O sysmon só despeja as linhas quando termina (saída em bloco), então rodamos rajadas curtas
// de 3 s, uma atrás da outra, e alimentamos a média móvel com as linhas de cada rajada.
function rodaSysmon() {
  if (parando) return;
  const exe = exePath();
  if (!fs.existsSync(exe)) { sys = fallbackNode(); atualizaSys(); return; }
  let saida = '';
  proc = spawn(exe, ['sysmon', '3'], { windowsHide: true });
  proc.stdout.on('data', (d) => { saida += d.toString('utf8'); });
  proc.on('error', () => {});
  proc.on('exit', () => {
    proc = null;
    let achou = false;
    for (const l of saida.split(/\r?\n/)) {
      const p = parseLinha(l);
      if (!p) continue;
      achou = true;
      // mesma suavização do notch: peso 0,4 por segundo (CPU e atividade do disco)
      suave.cpu = suave.cpu == null ? p.cpu : suave.cpu * 0.6 + p.cpu * 0.4;
      if (p.disco) suave.busy = suave.busy == null ? p.disco.busy : suave.busy * 0.6 + p.disco.busy * 0.4;
      sys = { ...p, cpu: Math.round(suave.cpu), disco: p.disco && { ...p.disco, busy: Math.round(suave.busy) } };
    }
    if (achou) { tentativas = 0; atualizaSys(); } else { tentativas++; }
    if (!parando) setTimeout(rodaSysmon, achou ? 0 : Math.min(60000, 3000 * tentativas));
  });
}

function fallbackNode() {
  const total = os.totalmem();
  const livre = os.freemem();
  return {
    cpu: null,
    ram: Math.round(((total - livre) / total) * 100),
    ramUsada: ((total - livre) / 2 ** 30).toFixed(1),
    ramTotal: (total / 2 ** 30).toFixed(1),
    disco: null,
    rede: null,
  };
}

function md(linhas) {
  const m = new vscode.MarkdownString();
  m.appendMarkdown(linhas.join('\n'));
  return m;
}

function atualizaSys() {
  if (!sys) return;
  if (sys.cpu != null) {
    const it = item('cpu', 107);
    it.text = med('CPU', sys.cpu);
    pinta(it, sys.cpu);
    it.tooltip = md(['**Processador**', '', `Uso: ${sys.cpu}% (mesma conta do Codenotch)`]);
    it.show();
  }
  const r = item('ram', 106);
  r.text = med('RAM', sys.ram);
  pinta(r, sys.ram);
  r.tooltip = md(['**Memória RAM**', '', `${sys.ramUsada} de ${sys.ramTotal} GB em uso (${sys.ram}%)`]);
  r.show();
  if (sys.disco) {
    const s = item('ssd', 104);
    // Mesma medida do Gerenciador de Tarefas ("Tempo de atividade" do disco), não o espaço ocupado.
    s.text = med('SSD', sys.disco.busy);
    pinta(s, sys.disco.busy);
    s.tooltip = md([
      `**SSD ${sys.disco.letra}**`, '',
      `Atividade agora: ${sys.disco.busy}% (igual ao Gerenciador de Tarefas)`,
      `Espaço usado: ${sys.disco.usado} de ${sys.disco.total} GB (${sys.disco.pct}%)`,
      sys.rede ? `Internet: ↓ ${sys.rede.down.toFixed(1)} Mb/s · ↑ ${sys.rede.up.toFixed(1)} Mb/s` : '',
    ]);
    s.show();
  }
}

// ---------- VRAM (memória de vídeo) ----------
// Placa integrada (Intel Iris Xe) não tem VRAM própria: o Windows usa memória compartilhada,
// com teto de metade da RAM. É o mesmo "Memória de GPU" do Gerenciador de Tarefas.
function leVram() {
  if (parando) return;
  execFile(sys32('typeperf.exe'), ['\\GPU Adapter Memory(*)\\Dedicated Usage', '\\GPU Adapter Memory(*)\\Shared Usage', '\\GPU Engine(*)\\Utilization Percentage', '-sc', '1'],
    { windowsHide: true, cwd: os.tmpdir(), timeout: 12000, maxBuffer: 8 * 1024 * 1024 }, (err, out) => {
      if (err || !out) return;
      const linhas = out.split(/\r?\n/).filter((l) => l.startsWith('"'));
      if (linhas.length < 2) return;
      const cab = linhas[0].split('","');
      const val = linhas[1].split('","');
      let ded = 0;
      let comp = 0;
      const motores = new Map(); // (placa + tipo de motor) -> soma entre processos
      for (let i = 1; i < cab.length; i++) {
        const v = parseFloat(String(val[i]).replace(/"/g, ''));
        if (Number.isNaN(v)) continue;
        if (/Dedicated Usage/i.test(cab[i])) ded += v;
        else if (/Shared Usage/i.test(cab[i])) comp += v;
        else if (/GPU Engine/i.test(cab[i])) {
          const m = /luid_(\w+_\w+)_phys_\d+_eng_\d+_engtype_(\w*)/i.exec(cab[i]);
          if (m) motores.set(`${m[1]}/${m[2]}`, (motores.get(`${m[1]}/${m[2]}`) || 0) + v);
        }
      }
      // Gerenciador de Tarefas: uso da GPU = o motor mais ocupado (3D, Compute, Copy, Vídeo...)
      const uso = Math.min(100, Math.max(0, ...motores.values()));
      vram = { ded, comp, uso };
      atualizaVram();
    });
}

function atualizaVram() {
  if (!vram) return;
  const usadoGB = (vram.ded + vram.comp) / 2 ** 30;
  const tetoGB = os.totalmem() / 2 ** 31 + vram.ded / 2 ** 30; // metade da RAM compartilhável
  const pct = Math.min(100, (usadoGB / tetoGB) * 100);
  const g = item('gpu', 105.5);
  g.text = med('GPU', vram.uso);
  pinta(g, vram.uso);
  g.tooltip = md(['**GPU (processamento)**', '', `Uso: ${Math.round(vram.uso)}% (igual ao "GPU 0" do Gerenciador de Tarefas)`]);
  g.show();
  const it = item('vram', 105);
  it.text = med('VRAM', pct);
  pinta(it, pct);
  it.tooltip = md([
    '**Memória de vídeo**', '',
    `Em uso: ${usadoGB.toFixed(1)} GB de ~${tetoGB.toFixed(1)} GB (${Math.round(pct)}%)`,
    `Dedicada: ${(vram.ded / 2 ** 30).toFixed(1)} GB · Compartilhada: ${(vram.comp / 2 ** 30).toFixed(1)} GB`,
    '',
    '_Placa integrada: usa a RAM; o teto é estimado em metade da RAM._',
  ]);
  it.show();
}

// ---------- IAs ----------
function curto(w) {
  const t = `${w.id} ${w.label}`.toLowerCase();
  if (/scoped|model/.test(t)) return null; // limite por modelo fica só no balão
  if (/primary|five|5h|session|sess/.test(t)) return 'sessão';
  if (/secondary|week|semana/.test(t)) return 'sem';
  return null;
}

function reinicia(ms) {
  if (!ms) return '';
  const min = Math.round((ms - Date.now()) / 60000);
  if (min <= 0) return 'agora';
  if (min < 60) return `${min} min`;
  if (min < 48 * 60) return `${Math.floor(min / 60)}h${String(min % 60).padStart(2, '0')}`;
  return `${Math.round(min / 1440)} d`;
}

function atualizaIAs() {
  const c = lerJson('config.json') || {};
  // Só Claude e GPT (Codex); outros provedores do Codenotch (Antigravity etc.) não entram na barra.
  const ativos = Array.isArray(c.notch_providers) && c.notch_providers.length ? c.notch_providers : ['claude', 'codex'];
  const ids = ['claude', 'codex'].filter((x) => ativos.includes(x));
  let prio = 110;
  for (const id of ids) {
    const d = lerJson(ARQUIVO[id] || `${id}.json`);
    const it = item(`ia-${id}`, prio--);
    if (!d || !Array.isArray(d.windows)) { it.hide(); continue; }
    const nome = NOMES[id] || id;
    const velho = d.fetched_at && Date.now() - d.fetched_at > 15 * 60000;
    const ruim = d.status !== 'ok' || velho;
    const mostradas = d.windows.map((w) => ({ w, k: curto(w) })).filter((x) => x.k);
    let texto;
    let maximo = 0;
    if (mostradas.length) {
      const todasZero = mostradas.every((x) => !x.w.used);
      if (ruim && todasZero) { texto = '?'; } else {
        texto = mostradas.map((x) => `${x.k} ${Math.round(x.w.used * 100)}%`).join(' / ');
        maximo = Math.max(...mostradas.map((x) => x.w.used * 100));
      }
    } else {
      const w = d.windows[0];
      texto = w && w.count != null ? `${w.count} req` : '?';
    }
    it.text = `${nome}: ${texto}${ruim && texto !== '?' ? ' ~' : ''}`;
    if (ruim) { it.backgroundColor = undefined; it.color = undefined; } else { pinta(it, maximo); }
    const linhas = [`**${nome}**${d.note ? ` · ${d.note}` : ''}`, ''];
    for (const w of d.windows) {
      const quanto = w.count != null && w.used === 0 && !w.resets_at ? `${w.count} req` : `${Math.round(w.used * 100)}%`;
      linhas.push(`- ${w.label}: ${quanto}${w.resets_at ? ` (reinicia em ${reinicia(w.resets_at)})` : ''}`);
    }
    if (ruim) linhas.push('', `_Dado ${velho ? 'desatualizado' : 'não confirmado'} (${d.status}). Clique para reler._`);
    it.tooltip = md(linhas);
    it.show();
  }
  for (const [chave, it] of itens) if (chave.startsWith('ia-') && !ids.includes(chave.slice(3))) it.hide();
}

// ---------- portas de localhost abertas ----------
const DEV = new Set(['node', 'bun', 'deno', 'python', 'pythonw', 'py', 'java', 'javaw', 'dotnet', 'php', 'ruby', 'php-cgi',
  'uvicorn', 'gunicorn', 'postgres', 'mysqld', 'mongod', 'redis-server', 'com.docker.backend', 'wslrelay', 'vite', 'next-server']);
let portas = [];

function lePortas() {
  if (parando) return;
  execFile(sys32('netstat.exe'), ['-ano', '-p', 'tcp'], { windowsHide: true, cwd: os.tmpdir(), timeout: 8000, maxBuffer: 4 * 1024 * 1024 }, (e1, out1) => {
    if (e1 || !out1) return;
    const cand = new Map(); // porta -> pid
    for (const l of out1.split(/\r?\n/)) {
      const m = /^\s*TCP\s+(\S+):(\d+)\s+\S+:0\s+(\S+)\s+(\d+)\s*$/.exec(l);
      if (!m || !/LISTEN|ESCUT/i.test(m[3])) continue;
      const [, host, porta, , pid] = m;
      if (!['127.0.0.1', '0.0.0.0', '[::1]', '[::]'].includes(host)) continue;
      const n = +porta;
      if (n < 1024 || n >= 49152) continue;
      if (!cand.has(n)) cand.set(n, +pid);
    }
    if (!cand.size) { portas = []; atualizaPortas(); return; }
    execFile(sys32('tasklist.exe'), ['/fo', 'csv', '/nh'], { windowsHide: true, cwd: os.tmpdir(), timeout: 8000, maxBuffer: 4 * 1024 * 1024 }, (e2, out2) => {
      if (e2 || !out2) return;
      const nomes = new Map();
      for (const l of out2.split(/\r?\n/)) {
        const m = /^"([^"]+)","(\d+)"/.exec(l);
        if (m) nomes.set(+m[2], m[1].replace(/\.exe$/i, ''));
      }
      portas = [...cand.entries()]
        .map(([porta, pid]) => ({ porta, nome: nomes.get(pid) || '?' }))
        .filter((x) => DEV.has(x.nome.toLowerCase()))
        .sort((a, b) => a.porta - b.porta);
      atualizaPortas();
    });
  });
}

function atualizaPortas() {
  const it = item('portas', 120);
  if (!portas.length) { it.hide(); return; }
  it.text = `$(globe) localhost ${portas.map((p) => `:${p.porta}`).join(' ')}`;
  it.color = '#38bdf8';
  it.backgroundColor = undefined;
  it.command = 'codenotch.portas';
  it.tooltip = md(['**Servidores locais abertos**', '', ...portas.map((p) => `- http://localhost:${p.porta} — ${p.nome}`), '', '_Clique para abrir no navegador._']);
  it.show();
}

async function abrePorta() {
  if (!portas.length) return;
  const esc = portas.length === 1
    ? portas[0]
    : (await vscode.window.showQuickPick(portas.map((p) => ({ label: `localhost:${p.porta}`, description: p.nome, p })), { placeHolder: 'Abrir qual porta?' }))?.p;
  if (esc) vscode.env.openExternal(vscode.Uri.parse(`http://localhost:${esc.porta}`));
}

function atualizaTudo() { atualizaSys(); atualizaVram(); atualizaIAs(); leVram(); lePortas(); }

function activate(ctx) {
  ctx.subscriptions.push(vscode.commands.registerCommand('codenotch.atualizar', atualizaTudo), vscode.commands.registerCommand('codenotch.portas', abrePorta));
  rodaSysmon();
  atualizaIAs();
  leVram();
  lePortas();
  timers.push(setInterval(atualizaIAs, 5000), setInterval(leVram, 5000), setInterval(lePortas, 3000));
}

function deactivate() {
  parando = true;
  for (const t of timers) clearInterval(t);
  if (proc) { try { proc.kill(); } catch { /* já saiu */ } }
  for (const it of itens.values()) it.dispose();
}

module.exports = { activate, deactivate };
