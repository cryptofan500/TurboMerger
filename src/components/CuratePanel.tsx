// Curate panel (T2-1/T2-2/T2-7): scan-first workflow. Three tabs over one
// selection model — a tri-state checkbox file tree with per-file token
// counts, a click-to-exclude token treemap, and a skip-report drill-in with
// per-file "include anyway" rescue. Selection state lives in App and is
// persisted per project.

import { useEffect, useMemo, useRef, useState } from "react";

export interface ScanEntry {
  path: string;
  size: number;
  tokens: number;
}
export interface SkipEntry {
  path: string;
  reason: string;
}
export interface ScanReport {
  root: string;
  included: ScanEntry[];
  skipped: SkipEntry[];
  total_tokens: number;
  duration_ms: number;
}

interface Props {
  report: ScanReport;
  excluded: Set<string>;
  forceInclude: Set<string>;
  budgetTokens: number;
  onSetExcluded: (next: Set<string>) => void;
  onToggleForce: (path: string) => void;
  onClose: () => void;
}

// Validated categorical palette (dataviz skill, dark surface #0d1117):
// fixed slot order, assigned to top-level dirs by token share at scan time
// and never re-ranked afterwards. Overflow folds into gray.
const PALETTE = [
  "#3987e5",
  "#199e70",
  "#c98500",
  "#008300",
  "#9085e9",
  "#e66767",
  "#d55181",
  "#d95926",
];
const OTHER_COLOR = "#8b949e";

const fmtTokens = (n: number) =>
  n < 1000 ? `${n}` : n < 1e6 ? `${(n / 1000).toFixed(n < 10000 ? 1 : 0)}k` : `${(n / 1e6).toFixed(2)}M`;

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

interface DirNode {
  name: string;
  path: string; // "" = root
  dirs: DirNode[];
  files: ScanEntry[];
  tokens: number;
  allFiles: string[]; // descendant file paths
}

function buildTree(entries: ScanEntry[]): DirNode {
  interface Builder {
    name: string;
    path: string;
    dirs: Map<string, Builder>;
    files: ScanEntry[];
  }
  const root: Builder = { name: "", path: "", dirs: new Map(), files: [] };
  for (const e of entries) {
    const parts = e.path.split("/");
    let cur = root;
    for (let i = 0; i < parts.length - 1; i++) {
      const p = parts.slice(0, i + 1).join("/");
      let next = cur.dirs.get(parts[i]);
      if (!next) {
        next = { name: parts[i], path: p, dirs: new Map(), files: [] };
        cur.dirs.set(parts[i], next);
      }
      cur = next;
    }
    cur.files.push(e);
  }
  const finalize = (b: Builder): DirNode => {
    const dirs = [...b.dirs.values()].map(finalize).sort((a, c) => a.name.localeCompare(c.name));
    const files = [...b.files].sort((a, c) => a.path.localeCompare(c.path));
    const tokens = dirs.reduce((s, d) => s + d.tokens, 0) + files.reduce((s, f) => s + f.tokens, 0);
    const allFiles = [...dirs.flatMap((d) => d.allFiles), ...files.map((f) => f.path)];
    return { name: b.name, path: b.path, dirs, files, tokens, allFiles };
  };
  return finalize(root);
}

// ---------------------------------------------------------------------------
// Tree tab
// ---------------------------------------------------------------------------

function TriCheckbox({
  state,
  onChange,
}: {
  state: "on" | "off" | "mixed";
  onChange: () => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = state === "mixed";
  }, [state]);
  return <input ref={ref} type="checkbox" checked={state === "on"} onChange={onChange} />;
}

function DirRow({
  node,
  depth,
  excluded,
  expanded,
  toggleExpand,
  toggleDir,
  toggleFile,
}: {
  node: DirNode;
  depth: number;
  excluded: Set<string>;
  expanded: Set<string>;
  toggleExpand: (path: string) => void;
  toggleDir: (node: DirNode) => void;
  toggleFile: (path: string) => void;
}) {
  const isOpen = expanded.has(node.path);
  const exCount = node.allFiles.reduce((n, f) => n + (excluded.has(f) ? 1 : 0), 0);
  const state: "on" | "off" | "mixed" =
    exCount === 0 ? "on" : exCount === node.allFiles.length ? "off" : "mixed";
  const selTokens = node.tokens; // shown as full dir weight; selection detail in summary
  return (
    <>
      <div className="tree-row" style={{ paddingLeft: depth * 18 }}>
        <button className="tree-arrow" onClick={() => toggleExpand(node.path)}>
          {isOpen ? "▾" : "▸"}
        </button>
        <TriCheckbox state={state} onChange={() => toggleDir(node)} />
        <span className="tree-name tree-dir-name" onClick={() => toggleExpand(node.path)}>
          {node.name || "."}/
        </span>
        <span className="tree-tokens">
          {exCount > 0 && exCount < node.allFiles.length ? `${node.allFiles.length - exCount}/${node.allFiles.length} · ` : ""}
          ~{fmtTokens(selTokens)}
        </span>
      </div>
      {isOpen && (
        <>
          {node.dirs.map((d) => (
            <DirRow
              key={d.path}
              node={d}
              depth={depth + 1}
              excluded={excluded}
              expanded={expanded}
              toggleExpand={toggleExpand}
              toggleDir={toggleDir}
              toggleFile={toggleFile}
            />
          ))}
          {node.files.map((f) => (
            <div className="tree-row" style={{ paddingLeft: (depth + 1) * 18 + 22 }} key={f.path}>
              <input
                type="checkbox"
                checked={!excluded.has(f.path)}
                onChange={() => toggleFile(f.path)}
              />
              <span
                className={`tree-name${excluded.has(f.path) ? " tree-excluded" : ""}`}
                onClick={() => toggleFile(f.path)}
              >
                {f.path.split("/").pop()}
              </span>
              <span className="tree-tokens">~{fmtTokens(f.tokens)}</span>
            </div>
          ))}
        </>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// Treemap tab (canvas, squarified)
// ---------------------------------------------------------------------------

interface Tile {
  x: number;
  y: number;
  w: number;
  h: number;
  path: string;
  label: string;
  tokens: number;
  isDir: boolean;
  color: string;
  excluded: boolean;
}

function squarify(
  items: { path: string; label: string; tokens: number; isDir: boolean; color: string; excluded: boolean }[],
  x: number,
  y: number,
  w: number,
  h: number,
  out: Tile[]
) {
  const total = items.reduce((s, i) => s + i.tokens, 0);
  if (total <= 0 || w <= 1 || h <= 1) return;
  let rest = [...items];
  let rx = x, ry = y, rw = w, rh = h, restTotal = total;
  while (rest.length > 0) {
    const horizontal = rw >= rh; // lay the row along the shorter side
    const side = horizontal ? rh : rw;
    const area = (t: number) => (t / restTotal) * rw * rh;
    let row: typeof rest = [];
    let rowArea = 0;
    let worst = Infinity;
    for (const item of rest) {
      const testArea = rowArea + area(item.tokens);
      const rowLen = testArea / side;
      let w0 = Infinity;
      for (const r of [...row, item]) {
        const a = area(r.tokens);
        const ratio = Math.max((rowLen * rowLen * a) / (testArea * testArea) === 0 ? Infinity : 0, 0);
        void ratio;
        const tileSide = a / rowLen;
        w0 = Math.min(w0, Math.min(rowLen === 0 ? Infinity : tileSide / rowLen, tileSide === 0 ? Infinity : rowLen / tileSide));
      }
      const newWorst = 1 / w0;
      if (row.length === 0 || newWorst <= worst) {
        row.push(item);
        rowArea = testArea;
        worst = newWorst;
      } else {
        break;
      }
    }
    if (row.length === 0) row = [rest[0]];
    const rowLen = rowArea / side;
    let offset = 0;
    for (const r of row) {
      const a = area(r.tokens);
      const tileSide = rowLen === 0 ? 0 : a / rowLen;
      if (horizontal) {
        out.push({ x: rx, y: ry + offset, w: rowLen, h: tileSide, ...r });
      } else {
        out.push({ x: rx + offset, y: ry, w: tileSide, h: rowLen, ...r });
      }
      offset += tileSide;
    }
    if (horizontal) {
      rx += rowLen;
      rw -= rowLen;
    } else {
      ry += rowLen;
      rh -= rowLen;
    }
    restTotal -= row.reduce((s, r) => s + r.tokens, 0);
    rest = rest.slice(row.length);
  }
}

function findDir(root: DirNode, path: string): DirNode {
  if (path === "") return root;
  let cur = root;
  for (const part of path.split("/")) {
    const next = cur.dirs.find((d) => d.name === part);
    if (!next) return root;
    cur = next;
  }
  return cur;
}

function Treemap({
  root,
  colorOf,
  excluded,
  toggleFile,
}: {
  root: DirNode;
  colorOf: (path: string) => string;
  excluded: Set<string>;
  toggleFile: (path: string) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const tilesRef = useRef<Tile[]>([]);
  const [zoomPath, setZoomPath] = useState("");
  const [tip, setTip] = useState<{ x: number; y: number; text: string } | null>(null);

  const draw = () => {
    const canvas = canvasRef.current;
    const wrap = wrapRef.current;
    if (!canvas || !wrap) return;
    const cssW = wrap.clientWidth;
    const cssH = 320;
    const dpr = window.devicePixelRatio || 1;
    canvas.width = cssW * dpr;
    canvas.height = cssH * dpr;
    canvas.style.width = `${cssW}px`;
    canvas.style.height = `${cssH}px`;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, cssW, cssH);

    const node = findDir(root, zoomPath);
    const items = [
      ...node.dirs.map((d) => ({
        path: d.path,
        label: `${d.name}/`,
        tokens: Math.max(d.tokens, 1),
        isDir: true,
        color: colorOf(d.path),
        excluded: d.allFiles.length > 0 && d.allFiles.every((f) => excluded.has(f)),
      })),
      ...node.files.map((f) => ({
        path: f.path,
        label: f.path.split("/").pop() || f.path,
        tokens: Math.max(f.tokens, 1),
        isDir: false,
        color: colorOf(f.path),
        excluded: excluded.has(f.path),
      })),
    ].sort((a, b) => b.tokens - a.tokens);

    const tiles: Tile[] = [];
    squarify(items, 0, 0, cssW, cssH, tiles);
    tilesRef.current = tiles;

    for (const t of tiles) {
      // 2px surface gap between fills (the surface shows through).
      const gx = t.x + 1, gy = t.y + 1, gw = Math.max(t.w - 2, 0), gh = Math.max(t.h - 2, 0);
      ctx.globalAlpha = t.excluded ? 0.3 : t.isDir ? 0.85 : 0.65;
      ctx.fillStyle = t.color;
      ctx.fillRect(gx, gy, gw, gh);
      ctx.globalAlpha = 1;
      if (t.excluded && gw > 8 && gh > 8) {
        // hatch texture: excluded is never signaled by dimming alone
        ctx.save();
        ctx.beginPath();
        ctx.rect(gx, gy, gw, gh);
        ctx.clip();
        ctx.strokeStyle = "rgba(13,17,23,0.85)";
        ctx.lineWidth = 1.5;
        for (let d = -gh; d < gw; d += 8) {
          ctx.beginPath();
          ctx.moveTo(gx + d, gy + gh);
          ctx.lineTo(gx + d + gh, gy);
          ctx.stroke();
        }
        ctx.restore();
      }
      if (gw > 56 && gh > 16) {
        // labels wear text tokens, not the series color
        ctx.fillStyle = "rgba(240,246,252,0.92)";
        ctx.font = `${t.isDir ? "600 " : ""}11px "Segoe UI", sans-serif`;
        let label = t.label;
        while (label.length > 3 && ctx.measureText(label).width > gw - 10) {
          label = `${label.slice(0, -2)}…`;
        }
        ctx.fillText(label, gx + 5, gy + 13);
        if (gh > 30) {
          ctx.fillStyle = "rgba(240,246,252,0.6)";
          ctx.font = '10px "Segoe UI", sans-serif';
          ctx.fillText(`~${fmtTokens(t.tokens)}`, gx + 5, gy + 26);
        }
      }
    }
  };

  useEffect(() => {
    draw();
    const onResize = () => draw();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [root, zoomPath, excluded]);

  const hit = (e: React.MouseEvent): Tile | null => {
    const rect = canvasRef.current?.getBoundingClientRect();
    if (!rect) return null;
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    return (
      tilesRef.current.find((t) => x >= t.x && x < t.x + t.w && y >= t.y && y < t.y + t.h) || null
    );
  };

  const crumbs = zoomPath === "" ? [] : zoomPath.split("/");
  return (
    <div>
      <div className="treemap-crumbs">
        <button className="crumb" onClick={() => setZoomPath("")}>root</button>
        {crumbs.map((c, i) => (
          <span key={i}>
            {" / "}
            <button className="crumb" onClick={() => setZoomPath(crumbs.slice(0, i + 1).join("/"))}>
              {c}
            </button>
          </span>
        ))}
        <span className="treemap-hint"> — click a folder to zoom, a file to include/exclude</span>
      </div>
      <div ref={wrapRef} className="treemap-wrap">
        <canvas
          ref={canvasRef}
          onClick={(e) => {
            const t = hit(e);
            if (!t) return;
            if (t.isDir) setZoomPath(t.path);
            else toggleFile(t.path);
          }}
          onMouseMove={(e) => {
            const t = hit(e);
            if (!t) {
              setTip(null);
              return;
            }
            const rect = canvasRef.current!.getBoundingClientRect();
            setTip({
              x: e.clientX - rect.left,
              y: e.clientY - rect.top,
              text: `${t.path || "."}${t.isDir ? "/" : ""} — ~${fmtTokens(t.tokens)} tokens${t.excluded ? " (excluded)" : ""}`,
            });
          }}
          onMouseLeave={() => setTip(null)}
        />
        {tip && (
          <div
            className="treemap-tip"
            style={{ left: Math.min(tip.x + 12, (wrapRef.current?.clientWidth || 300) - 240), top: tip.y + 14 }}
          >
            {tip.text}
          </div>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Skip drill-in tab
// ---------------------------------------------------------------------------

function categorize(reason: string): string {
  const r = reason.toLowerCase();
  if (r.includes("gitignore")) return "Gitignored";
  if (r.includes("credential") || r.includes("sensitive") || r.includes("env file") || r.includes("secret")) return "Sensitive / credentials";
  if (r.includes("binary")) return "Binary";
  if (r.includes("large") || r.includes("exceeds")) return "Too large";
  if (r.includes("minified") || r.includes("lock") || r.includes("previous") || r.includes("output") || r.includes("artifact")) return "Artifacts / lockfiles";
  if (r.includes("unreadable")) return "Unreadable";
  if (r.includes("hidden") || r.includes("dotfile")) return "Hidden";
  if (r.includes("venv") || r.includes("virtual") || r.includes("node_modules") || r.includes("dependency")) return "Virtual envs / deps";
  return "Other";
}

function SkipList({
  skipped,
  forceInclude,
  onToggleForce,
}: {
  skipped: SkipEntry[];
  forceInclude: Set<string>;
  onToggleForce: (path: string) => void;
}) {
  const groups = useMemo(() => {
    const m = new Map<string, SkipEntry[]>();
    for (const s of skipped) {
      const cat = categorize(s.reason);
      const arr = m.get(cat) || [];
      arr.push(s);
      m.set(cat, arr);
    }
    return [...m.entries()].sort((a, b) => b[1].length - a[1].length);
  }, [skipped]);
  const [open, setOpen] = useState<Set<string>>(() => new Set(groups.slice(0, 1).map(([g]) => g)));

  if (skipped.length === 0) return <p className="hint">Nothing was skipped.</p>;
  return (
    <div className="skip-list">
      {groups.map(([cat, entries]) => (
        <div key={cat} className="skip-group">
          <button
            className="skip-group-head"
            onClick={() => {
              const next = new Set(open);
              if (next.has(cat)) next.delete(cat);
              else next.add(cat);
              setOpen(next);
            }}
          >
            {open.has(cat) ? "▾" : "▸"} {cat} ({entries.length})
          </button>
          {open.has(cat) && (
            <div>
              {cat === "Sensitive / credentials" && (
                <p className="skip-warning">
                  ⚠ Rescued sensitive files still pass merge-level safety (binary check,
                  credential-density exclusion, redaction) — but review before sharing.
                </p>
              )}
              {entries.map((s) => (
                <div className="skip-row" key={s.path}>
                  <span className="skip-path">{s.path}</span>
                  <span className="skip-reason">{s.reason}</span>
                  <button
                    className={`btn btn-tiny ${forceInclude.has(s.path) ? "btn-success" : "btn-secondary"}`}
                    onClick={() => onToggleForce(s.path)}
                  >
                    {forceInclude.has(s.path) ? "✓ will include" : "include anyway"}
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

export default function CuratePanel({
  report,
  excluded,
  forceInclude,
  budgetTokens,
  onSetExcluded,
  onToggleForce,
  onClose,
}: Props) {
  const [tab, setTab] = useState<"tree" | "map" | "skips">("tree");
  const tree = useMemo(() => buildTree(report.included), [report]);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set([""]));

  // Fixed categorical assignment: top-level dirs by token share AT SCAN TIME.
  const colorOf = useMemo(() => {
    const tops = [...tree.dirs].sort((a, b) => b.tokens - a.tokens);
    const slot = new Map<string, string>();
    tops.forEach((d, i) => slot.set(d.name, i < PALETTE.length ? PALETTE[i] : OTHER_COLOR));
    return (path: string) => {
      const top = path.split("/")[0];
      return slot.get(top) || OTHER_COLOR;
    };
  }, [tree]);

  const tokensByPath = useMemo(() => {
    const m = new Map<string, number>();
    for (const e of report.included) m.set(e.path, e.tokens);
    return m;
  }, [report]);

  const excludedTokens = useMemo(() => {
    let t = 0;
    for (const p of excluded) t += tokensByPath.get(p) || 0;
    return t;
  }, [excluded, tokensByPath]);

  const selectedTokens = report.total_tokens - excludedTokens;
  const selectedCount = report.included.length - excluded.size;
  const pct = budgetTokens > 0 ? (selectedTokens / budgetTokens) * 100 : 0;
  const barColor = pct <= 80 ? "var(--success)" : pct <= 100 ? "var(--warning)" : "var(--danger)";

  const toggleFile = (path: string) => {
    const next = new Set(excluded);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    onSetExcluded(next);
  };
  const toggleDir = (node: DirNode) => {
    const next = new Set(excluded);
    const exCount = node.allFiles.reduce((n, f) => n + (next.has(f) ? 1 : 0), 0);
    const excludeAll = exCount < node.allFiles.length; // any included → exclude all
    for (const f of node.allFiles) {
      if (excludeAll) next.add(f);
      else next.delete(f);
    }
    onSetExcluded(next);
  };
  const toggleExpand = (path: string) => {
    const next = new Set(expanded);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    setExpanded(next);
  };

  return (
    <section className="section curate-panel">
      <div className="curate-head">
        <div className="tabs">
          {(
            [
              ["tree", `Files (${report.included.length})`],
              ["map", "Token treemap"],
              ["skips", `Skipped (${report.skipped.length})`],
            ] as ["tree" | "map" | "skips", string][]
          ).map(([key, label]) => (
            <button
              key={key}
              className={`tab${tab === key ? " tab-active" : ""}`}
              onClick={() => setTab(key)}
            >
              {label}
            </button>
          ))}
        </div>
        <button className="btn btn-secondary btn-tiny" onClick={onClose}>Done</button>
      </div>

      <div className="curate-summary">
        <span>
          {selectedCount.toLocaleString()} of {report.included.length.toLocaleString()} files ·
          ~{fmtTokens(selectedTokens)} of ~{fmtTokens(report.total_tokens)} tokens selected
          {forceInclude.size > 0 && ` · +${forceInclude.size} rescued`}
        </span>
        <div className="budget-bar">
          <div
            className="budget-fill"
            style={{ width: `${Math.min(pct, 100)}%`, background: barColor }}
          />
        </div>
        <span className="budget-label" style={{ color: barColor }}>
          {pct.toFixed(0)}% of ~{fmtTokens(budgetTokens)} budget
        </span>
      </div>

      {tab === "tree" && (
        <div className="tree-scroll">
          <div className="tree-toolbar">
            <button className="btn btn-tiny btn-secondary" onClick={() => onSetExcluded(new Set())}>
              Select all
            </button>
            <button
              className="btn btn-tiny btn-secondary"
              onClick={() => onSetExcluded(new Set(report.included.map((e) => e.path)))}
            >
              Deselect all
            </button>
          </div>
          {tree.dirs.map((d) => (
            <DirRow
              key={d.path}
              node={d}
              depth={0}
              excluded={excluded}
              expanded={expanded}
              toggleExpand={toggleExpand}
              toggleDir={toggleDir}
              toggleFile={toggleFile}
            />
          ))}
          {tree.files.map((f) => (
            <div className="tree-row" style={{ paddingLeft: 22 }} key={f.path}>
              <input
                type="checkbox"
                checked={!excluded.has(f.path)}
                onChange={() => toggleFile(f.path)}
              />
              <span
                className={`tree-name${excluded.has(f.path) ? " tree-excluded" : ""}`}
                onClick={() => toggleFile(f.path)}
              >
                {f.path}
              </span>
              <span className="tree-tokens">~{fmtTokens(f.tokens)}</span>
            </div>
          ))}
        </div>
      )}

      {tab === "map" && (
        <Treemap root={tree} colorOf={colorOf} excluded={excluded} toggleFile={toggleFile} />
      )}

      {tab === "skips" && (
        <SkipList skipped={report.skipped} forceInclude={forceInclude} onToggleForce={onToggleForce} />
      )}
    </section>
  );
}
