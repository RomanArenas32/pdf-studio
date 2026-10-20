import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask, open, save } from "@tauri-apps/plugin-dialog";

interface PageSize {
  width: number;
  height: number;
}

interface DocInfo {
  path: string;
  pages: PageSize[];
  canUndo: boolean;
  canRedo: boolean;
}

/** Punto relativo a la página mostrada: (0, 0) arriba a la izquierda, (1, 1) abajo a la derecha. */
interface RelPoint {
  x: number;
  y: number;
}

interface RelRect {
  from: RelPoint;
  to: RelPoint;
}

/** Texto agregado con la app; `anchor` es el inicio de la línea base de la primera línea. */
interface TextInfo {
  lines: string[];
  size: number;
  color: string;
  anchor: RelPoint;
}

interface AnnotationHit {
  annotation: number;
  rect: RelRect;
  text: TextInfo | null;
}

/** Fragmento de texto original del PDF (puede abarcar varios objetos, p. ej. uno por letra). */
interface TextObjectHit {
  objects: number[];
  text: string;
  rect: RelRect;
  size: number;
  color: string;
  font: string;
}

interface TextEditResult {
  doc: DocInfo;
  fontReplaced: boolean;
}

type Tool = "select" | "text" | "edit-text" | "highlight" | "rect" | "pen" | "eraser" | "batch";

const THUMB_WIDTH = 140;
const ZOOM_STEPS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3];
const SVG_NS = "http://www.w3.org/2000/svg";
/** Posición de la línea base dentro de una línea de texto (Helvetica con line-height 1.2). */
const BASELINE_RATIO = 0.87;
const LINE_HEIGHT = 1.2;
const PDF_FILTER = [{ name: "PDF", extensions: ["pdf"] }];

const state = {
  doc: null as DocInfo | null,
  current: 0,
  zoomIndex: 2,
  dirty: false,
  tool: "select" as Tool,
  colors: { text: "#000000", highlight: "#ffd400", rect: "#e01e1e", pen: "#e01e1e" } as Record<
    string,
    string
  >,
  textSize: 14,
  strokeWidth: 2,
  // Se incrementa al reconstruir las miniaturas para descartar renders viejos.
  thumbsVersion: 0,
  renderSeq: 0,
  busy: false,
};

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const els = {
  open: $<HTMLButtonElement>("btn-open"),
  openEmpty: $<HTMLButtonElement>("btn-open-empty"),
  save: $<HTMLButtonElement>("btn-save"),
  saveAs: $<HTMLButtonElement>("btn-save-as"),
  undo: $<HTMLButtonElement>("btn-undo"),
  redo: $<HTMLButtonElement>("btn-redo"),
  prev: $<HTMLButtonElement>("btn-prev"),
  next: $<HTMLButtonElement>("btn-next"),
  zoomOut: $<HTMLButtonElement>("btn-zoom-out"),
  zoomIn: $<HTMLButtonElement>("btn-zoom-in"),
  rotateLeft: $<HTMLButtonElement>("btn-rotate-left"),
  rotateRight: $<HTMLButtonElement>("btn-rotate-right"),
  blank: $<HTMLButtonElement>("btn-blank"),
  insert: $<HTMLButtonElement>("btn-insert"),
  del: $<HTMLButtonElement>("btn-delete"),
  tools: $("tools"),
  toolOptions: $("tool-options"),
  color: $<HTMLInputElement>("opt-color"),
  size: $<HTMLSelectElement>("opt-size"),
  sizeWrap: $("opt-size-wrap"),
  width: $<HTMLInputElement>("opt-width"),
  widthWrap: $("opt-width-wrap"),
  pageIndicator: $("page-indicator"),
  zoomIndicator: $("zoom-indicator"),
  fileName: $("file-name"),
  thumbnails: $("thumbnails"),
  viewer: $("viewer"),
  empty: $("empty"),
  pageWrap: $("page-wrap"),
  page: $<HTMLImageElement>("page"),
  overlay: document.getElementById("overlay") as unknown as SVGSVGElement,
  toast: $("toast"),
  batch: $<HTMLButtonElement>("btn-batch"),
  batchPanel: $("batch-panel"),
  batchClose: $<HTMLButtonElement>("batch-close"),
  batchFields: $("batch-fields"),
  batchLoad: $<HTMLButtonElement>("batch-load"),
  batchTableInfo: $("batch-table-info"),
  batchPreviewNav: $("batch-preview-nav"),
  batchPrev: $<HTMLButtonElement>("batch-prev"),
  batchNext: $<HTMLButtonElement>("batch-next"),
  batchRowLabel: $("batch-row-label"),
  batchPattern: $<HTMLInputElement>("batch-pattern"),
  batchRun: $<HTMLButtonElement>("batch-run"),
  batchProgress: $("batch-progress"),
  batchProgressBar: $<HTMLProgressElement>("batch-progress-bar"),
  batchProgressLabel: $("batch-progress-label"),
  batchCancel: $<HTMLButtonElement>("batch-cancel"),
  batchResult: $("batch-result"),
};

// ---------- Utilidades ----------

async function renderPng(index: number, width: number): Promise<string> {
  const buf = await invoke<ArrayBuffer>("render_page", { index, width: Math.round(width) });
  return URL.createObjectURL(new Blob([buf], { type: "image/png" }));
}

function setImage(img: HTMLImageElement, url: string) {
  if (img.src.startsWith("blob:")) URL.revokeObjectURL(img.src);
  img.src = url;
}

let toastTimer = 0;
function showError(e: unknown, kind: "error" | "info" = "error") {
  els.toast.textContent = String(e);
  els.toast.dataset.kind = kind;
  els.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (els.toast.hidden = true), 4000);
}

function showInfo(message: string) {
  showError(message, "info");
}

function baseName(path: string) {
  return path.split(/[\\/]/).pop() ?? path;
}

function isPdf(path: string) {
  return path.toLowerCase().endsWith(".pdf");
}

/** Ancho en px CSS con el que se muestra la página (las medidas ya incluyen la rotación). */
function displayWidth(index: number) {
  return state.doc!.pages[index].width * ZOOM_STEPS[state.zoomIndex] * (96 / 72);
}

/** Píxeles CSS por punto PDF en la página actual. */
function pxPerPt() {
  return ZOOM_STEPS[state.zoomIndex] * (96 / 72);
}

function relPoint(e: PointerEvent | MouseEvent): RelPoint {
  const r = els.page.getBoundingClientRect();
  return {
    x: Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)),
    y: Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)),
  };
}

function toPx(p: RelPoint) {
  const r = els.page.getBoundingClientRect();
  return { x: p.x * r.width, y: p.y * r.height };
}

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number>) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  return el;
}

function clearOverlay() {
  els.overlay.replaceChildren();
  selection = null;
  hoverBox = null;
}

/** Dibuja un contorno en la capa de dibujo alrededor de un rectángulo relativo. */
function outline(r: RelRect, className: string) {
  const a = toPx(r.from);
  const b = toPx(r.to);
  const pad = 2;
  const box = svg("rect", {
    class: className,
    x: a.x - pad,
    y: a.y - pad,
    width: b.x - a.x + pad * 2,
    height: b.y - a.y + pad * 2,
  });
  els.overlay.append(box);
  return box;
}

// ---------- Render ----------

async function renderCurrent() {
  if (!state.doc) return;
  const seq = ++state.renderSeq;
  const index = state.current;
  const cssWidth = displayWidth(index);
  const width = cssWidth * window.devicePixelRatio;
  const url = batchPreviewing() ? await renderBatchPreview(index, width) : await renderPng(index, width);
  if (seq !== state.renderSeq) {
    URL.revokeObjectURL(url);
    return;
  }
  setImage(els.page, url);
  els.page.style.width = `${cssWidth}px`;
  els.pageWrap.hidden = false;
  clearOverlay();
  drawBatchFields();
}

let thumbObserver: IntersectionObserver | null = null;

function renderThumb(item: HTMLElement) {
  const version = state.thumbsVersion;
  const index = Number(item.dataset.index);
  renderPng(index, THUMB_WIDTH * window.devicePixelRatio)
    .then((url) => {
      if (version !== state.thumbsVersion) return URL.revokeObjectURL(url);
      setImage(item.querySelector("img")!, url);
    })
    .catch(showError);
}

function buildThumbnails() {
  thumbObserver?.disconnect();
  els.thumbnails.querySelectorAll("img").forEach((img) => setImage(img, ""));
  els.thumbnails.replaceChildren();
  state.thumbsVersion++;
  if (!state.doc) return;

  thumbObserver = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        thumbObserver!.unobserve(entry.target);
        renderThumb(entry.target as HTMLElement);
      }
    },
    { root: els.thumbnails, rootMargin: "200px" },
  );

  state.doc.pages.forEach((size, index) => {
    const item = document.createElement("button");
    item.className = "thumb";
    item.dataset.index = String(index);
    item.title = "Arrastra para reordenar";
    const img = document.createElement("img");
    img.draggable = false;
    img.style.aspectRatio = `${size.width} / ${size.height}`;
    const label = document.createElement("span");
    label.textContent = String(index + 1);
    item.append(img, label);
    setupThumbDrag(item, index);
    els.thumbnails.append(item);
    thumbObserver!.observe(item);
  });
}

function refreshThumb(index: number) {
  const item = els.thumbnails.querySelector<HTMLElement>(`.thumb[data-index="${index}"]`);
  if (item) renderThumb(item);
}

function updateUi() {
  const doc = state.doc;
  const has = doc !== null;
  els.empty.hidden = has;
  if (!has) els.pageWrap.hidden = true;

  const docButtons = [
    els.saveAs,
    els.zoomIn,
    els.zoomOut,
    els.rotateLeft,
    els.rotateRight,
    els.blank,
    els.insert,
    ...els.tools.querySelectorAll("button"),
  ];
  for (const b of docButtons) b.disabled = !has || state.busy;
  els.save.disabled = !has || !state.dirty || state.busy;
  els.undo.disabled = !has || !doc.canUndo || state.busy;
  els.redo.disabled = !has || !doc.canRedo || state.busy;
  els.del.disabled = !has || doc.pages.length <= 1 || state.busy;
  els.prev.disabled = !has || state.current === 0;
  els.next.disabled = !has || state.current >= doc.pages.length - 1;
  els.zoomOut.disabled ||= state.zoomIndex === 0;
  els.zoomIn.disabled ||= state.zoomIndex === ZOOM_STEPS.length - 1;
  els.batch.disabled = !has || state.busy || batch.active;
  // En modo serie el documento es la plantilla: no se edita para no invalidar los campos.
  if (batch.active) {
    const editing = [
      ...els.tools.querySelectorAll("button"),
      els.rotateLeft,
      els.rotateRight,
      els.blank,
      els.insert,
      els.del,
      els.undo,
      els.redo,
    ];
    for (const b of editing) b.disabled = true;
  }

  els.pageIndicator.textContent = has ? `${state.current + 1} / ${doc.pages.length}` : "–";
  els.zoomIndicator.textContent = `${Math.round(ZOOM_STEPS[state.zoomIndex] * 100)}%`;

  const name = has ? baseName(doc.path) : "";
  els.fileName.textContent = name + (state.dirty ? " •" : "");
  els.fileName.title = has ? doc.path : "";
  getCurrentWindow()
    .setTitle(has ? `${state.dirty ? "• " : ""}${name} — PDF Studio` : "PDF Studio")
    .catch(() => {});

  els.thumbnails.querySelectorAll<HTMLElement>(".thumb").forEach((t) => {
    const active = Number(t.dataset.index) === state.current;
    t.classList.toggle("active", active);
    if (active) t.setAttribute("aria-current", "page");
    else t.removeAttribute("aria-current");
  });
  els.thumbnails.querySelector(".thumb.active")?.scrollIntoView({ block: "nearest" });
}

/**
 * Aplica un `DocInfo` nuevo del backend y vuelve a pintar.
 * Con `page` solo se refresca esa miniatura (ediciones que no cambian la estructura).
 */
function applyDoc(doc: DocInfo, opts: { dirty: boolean; page?: number }) {
  state.doc = doc;
  state.dirty = opts.dirty;
  state.current = Math.max(0, Math.min(state.current, doc.pages.length - 1));
  if (opts.page === undefined) buildThumbnails();
  else refreshThumb(opts.page);
  updateUi();
  return renderCurrent().catch(showError);
}

// ---------- Acciones de documento ----------

function confirmDiscard() {
  return ask("Hay cambios sin guardar. ¿Descartarlos?", { title: "PDF Studio", kind: "warning" });
}

async function openPath(path: string) {
  if (batch.running) return;
  closeBatch();
  try {
    const doc = await invoke<DocInfo>("open_pdf", { path });
    state.current = 0;
    await applyDoc(doc, { dirty: false });
    els.viewer.scrollTo(0, 0);
  } catch (e) {
    showError(e);
  }
}

async function openDocument() {
  if (state.dirty && !(await confirmDiscard())) return;
  const path = await open({ multiple: false, filters: PDF_FILTER });
  if (path) await openPath(path);
}

async function saveDocument(saveAs: boolean) {
  if (!state.doc) return;
  let path: string | null = null;
  if (saveAs) {
    path = await save({ defaultPath: state.doc.path, filters: PDF_FILTER });
    if (!path) return;
  }
  try {
    state.doc = await invoke<DocInfo>("save_pdf", { path });
    state.dirty = false;
    updateUi();
  } catch (e) {
    showError(e);
  }
}

/**
 * Ejecuta un comando de edición; evita que se encimen dos ediciones a la vez.
 * Devuelve la respuesta del backend, o `null` si falló.
 */
async function edit<T extends DocInfo | { doc: DocInfo } = DocInfo>(
  command: string,
  args: Record<string, unknown>,
  page?: number,
): Promise<T | null> {
  if (state.busy) return null;
  if (batch.active) {
    showInfo("Cierra «Generar en serie» para editar el documento.");
    return null;
  }
  state.busy = true;
  updateUi();
  try {
    const result = await invoke<T>(command, args);
    state.busy = false;
    await applyDoc("doc" in result ? result.doc : result, { dirty: true, page });
    return result;
  } catch (e) {
    state.busy = false;
    updateUi();
    showError(e);
    return null;
  }
}

async function history(command: "undo" | "redo") {
  if (!state.doc || (command === "undo" ? !state.doc.canUndo : !state.doc.canRedo)) return;
  cancelTextInput();
  clearSelection();
  await edit(command, {});
}

async function insertPdfs(paths: string[]) {
  for (const path of paths) {
    const at = state.current + 1;
    const before = state.doc!.pages.length;
    if (!(await edit("insert_pdf", { path, at }))) return;
    // Queda seleccionada la última página insertada.
    state.current = at + state.doc!.pages.length - before - 1;
    updateUi();
    await renderCurrent();
  }
}

async function insertDocument() {
  if (!state.doc) return;
  const picked = await open({ multiple: true, filters: PDF_FILTER });
  if (picked) await insertPdfs(picked);
}

async function insertBlankPage() {
  if (!state.doc) return;
  const at = state.current + 1;
  if (await edit("insert_blank_page", { at, like: state.current })) goTo(at);
}

function goTo(index: number) {
  if (!state.doc) return;
  const clamped = Math.max(0, Math.min(index, state.doc.pages.length - 1));
  if (clamped === state.current) return;
  finishTextInput();
  state.current = clamped;
  updateUi();
  els.viewer.scrollTo(0, 0);
  renderCurrent().catch(showError);
}

function zoom(delta: number) {
  const next = Math.max(0, Math.min(state.zoomIndex + delta, ZOOM_STEPS.length - 1));
  if (!state.doc || next === state.zoomIndex) return;
  finishTextInput();
  state.zoomIndex = next;
  updateUi();
  renderCurrent().catch(showError);
}

// ---------- Reordenar miniaturas ----------

let suppressClick = false;
const dropIndicator = document.createElement("div");
dropIndicator.className = "drop-indicator";

/** Posición de inserción (0..n) según la altura del puntero sobre las miniaturas. */
function dropIndexAt(clientY: number) {
  const thumbs = [...els.thumbnails.querySelectorAll<HTMLElement>(".thumb")];
  const i = thumbs.findIndex((t) => {
    const r = t.getBoundingClientRect();
    return clientY < r.top + r.height / 2;
  });
  return i === -1 ? thumbs.length : i;
}

function showDropIndicator(index: number) {
  const thumbs = els.thumbnails.querySelectorAll<HTMLElement>(".thumb");
  const gap = 6;
  const top =
    index < thumbs.length
      ? thumbs[index].offsetTop - gap
      : thumbs[thumbs.length - 1].offsetTop + thumbs[thumbs.length - 1].offsetHeight + gap;
  dropIndicator.style.top = `${top}px`;
  if (!dropIndicator.isConnected) els.thumbnails.append(dropIndicator);
}

function setupThumbDrag(item: HTMLElement, from: number) {
  item.addEventListener("click", () => {
    if (!suppressClick) goTo(from);
    suppressClick = false;
  });

  item.addEventListener("pointerdown", (down) => {
    if (down.button !== 0 || state.busy) return;
    const startY = down.clientY;
    let dragging = false;
    let target = from;
    let scrollTimer = 0;

    const onMove = (e: PointerEvent) => {
      if (!dragging && Math.abs(e.clientY - startY) < 6) return;
      if (!dragging) {
        dragging = true;
        item.classList.add("dragging");
        item.setPointerCapture(down.pointerId);
      }
      target = dropIndexAt(e.clientY);
      showDropIndicator(target);

      // Desplaza la lista si el puntero está cerca de los bordes.
      clearInterval(scrollTimer);
      const r = els.thumbnails.getBoundingClientRect();
      const dir = e.clientY < r.top + 40 ? -1 : e.clientY > r.bottom - 40 ? 1 : 0;
      if (dir) scrollTimer = window.setInterval(() => els.thumbnails.scrollBy(0, dir * 12), 16);
    };

    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      clearInterval(scrollTimer);
      if (!dragging) return;
      suppressClick = true;
      item.classList.remove("dragging");
      dropIndicator.remove();
      if (target === from || target === from + 1) return;
      movePage(from, target);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
  });
}

async function movePage(from: number, to: number) {
  const order = state.doc!.pages.map((_, i) => i);
  order.splice(from, 1);
  order.splice(to > from ? to - 1 : to, 0, from);
  const previous = state.current;
  if (await edit("reorder_pages", { order })) {
    state.current = order.indexOf(previous);
    updateUi();
    await renderCurrent();
  }
}

// ---------- Herramientas de anotación ----------

function setTool(tool: Tool) {
  if (!state.doc || (batch.active && tool !== "batch")) return;
  finishTextInput();
  clearSelection();
  clearHover();
  els.overlay.style.cursor = "";
  state.tool = tool;
  els.pageWrap.dataset.tool = tool;
  els.tools.querySelectorAll<HTMLButtonElement>("button").forEach((b) => {
    b.setAttribute("aria-pressed", String(b.dataset.tool === tool));
  });
  const hasColor = tool in state.colors;
  els.toolOptions.hidden = !hasColor;
  els.sizeWrap.hidden = tool !== "text";
  els.widthWrap.hidden = tool !== "rect" && tool !== "pen";
  if (hasColor) els.color.value = state.colors[tool];
}

let textInput: HTMLTextAreaElement | null = null;
/** Confirma el texto en edición (si hay). */
let commitText: (() => void) | null = null;

function cancelTextInput() {
  textInput?.remove();
  textInput = null;
  commitText = null;
}

function finishTextInput() {
  if (commitText) commitText();
  else cancelTextInput();
}

function autosize(ta: HTMLTextAreaElement) {
  ta.style.width = "1px";
  ta.style.height = "1px";
  ta.style.width = `${ta.scrollWidth + 4}px`;
  ta.style.height = `${ta.scrollHeight}px`;
}

function pageHeightPx() {
  return els.page.getBoundingClientRect().height;
}

/** Línea base de cada línea a partir de la esquina superior izquierda del bloque. */
function layoutLines(text: string, topLeft: RelPoint, fontPx: number) {
  const height = pageHeightPx();
  return text.split("\n").map((line, i) => ({
    text: line,
    x: topLeft.x,
    y: topLeft.y + (fontPx * (BASELINE_RATIO + i * LINE_HEIGHT)) / height,
  }));
}

function topLeftOf(info: TextInfo): RelPoint {
  const fontPx = info.size * pxPerPt();
  return { x: info.anchor.x, y: info.anchor.y - (fontPx * BASELINE_RATIO) / pageHeightPx() };
}

/** Nombre de fuente del PDF a CSS: "ABCDEF+Arial-BoldMT" → "Arial". */
function cssFont(pdfFont: string) {
  const family = pdfFont.replace(/^[A-Z]{6}\+/, "").split(/[-,]/)[0];
  return `"${family}", Helvetica, Arial, sans-serif`;
}

function setTextSize(size: number) {
  state.textSize = size;
  const value = String(size);
  if (![...els.size.options].some((o) => o.value === value)) {
    els.size.append(new Option(value, value));
  }
  els.size.value = value;
}

interface EditorOptions {
  /** Esquina superior izquierda del cuadro. */
  at: RelPoint;
  value: string;
  fontPx: number;
  color: string;
  fontFamily?: string;
  minWidth?: number;
  /** Tapa lo que hay debajo (al editar un texto existente). */
  opaque?: boolean;
  /** Enter siempre confirma; no se permiten saltos de línea. */
  singleLine?: boolean;
  /** El tamaño y color de la barra se aplican en vivo. */
  styled?: boolean;
  /** Fuerza la confirmación aunque el texto no haya cambiado. */
  styleChanged?: () => boolean;
  onCommit: (text: string) => Promise<unknown>;
}

function openTextEditor(opts: EditorOptions) {
  cancelTextInput();
  clearHover();
  const px = toPx(opts.at);
  const ta = document.createElement("textarea");
  ta.className = opts.opaque ? "text-input opaque" : "text-input";
  ta.rows = 1;
  ta.spellcheck = false;
  ta.value = opts.value;
  if (opts.styled) ta.dataset.styled = "";
  Object.assign(ta.style, {
    left: `${px.x}px`,
    top: `${px.y}px`,
    fontSize: `${opts.fontPx}px`,
    lineHeight: String(LINE_HEIGHT),
    color: opts.color,
    fontFamily: opts.fontFamily ?? "",
  });
  textInput = ta;
  els.pageWrap.append(ta);
  autosize(ta);
  if (opts.minWidth) ta.style.width = `${Math.max(opts.minWidth, ta.scrollWidth + 4)}px`;
  requestAnimationFrame(() => {
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
  });

  let done = false;
  const commit = async () => {
    if (done) return;
    done = true;
    commitText = null;
    const text = ta.value.replace(/\s+$/, "");
    const unchanged = text === opts.value.replace(/\s+$/, "") && !opts.styleChanged?.();
    if (!unchanged) await opts.onCommit(text);
    if (textInput === ta) cancelTextInput();
  };
  commitText = commit;

  ta.addEventListener("input", () => {
    autosize(ta);
    if (opts.minWidth) ta.style.width = `${Math.max(opts.minWidth, ta.scrollWidth + 4)}px`;
  });
  ta.addEventListener("blur", (e) => {
    // Cambiar el tamaño o el color en la barra no cierra el cuadro.
    const to = e.relatedTarget;
    if (opts.styled && to instanceof Node && els.toolOptions.contains(to)) return;
    commit();
  });
  ta.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") {
      done = true;
      cancelTextInput();
    } else if (e.key === "Enter" && (!e.shiftKey || opts.singleLine)) {
      e.preventDefault();
      commit();
    }
  });
}

function newText(at: RelPoint) {
  const index = state.current;
  openTextEditor({
    at,
    value: "",
    fontPx: state.textSize * pxPerPt(),
    color: state.colors.text,
    styled: true,
    onCommit: (text) =>
      edit(
        "add_text",
        {
          index,
          lines: layoutLines(text, at, state.textSize * pxPerPt()),
          size: state.textSize,
          color: state.colors.text,
        },
        index,
      ),
  });
}

/** Vuelve a abrir un texto agregado con la app para editarlo. */
function editOwnText(annotation: number, info: TextInfo) {
  const index = state.current;
  setTool("text");
  setTextSize(info.size);
  state.colors.text = info.color;
  els.color.value = info.color;
  const at = topLeftOf(info);
  openTextEditor({
    at,
    value: info.lines.join("\n"),
    fontPx: info.size * pxPerPt(),
    color: info.color,
    opaque: true,
    styled: true,
    styleChanged: () => state.textSize !== info.size || state.colors.text !== info.color,
    onCommit: (text) =>
      edit(
        "update_text",
        {
          index,
          annotation,
          lines: text ? layoutLines(text, at, state.textSize * pxPerPt()) : [],
          size: state.textSize,
          color: state.colors.text,
        },
        index,
      ),
  });
}

async function onTextToolClick(at: RelPoint) {
  const index = state.current;
  const hit = await invoke<AnnotationHit | null>("annotation_at", { index, at }).catch(() => null);
  if (index !== state.current) return;
  if (hit?.text) editOwnText(hit.annotation, hit.text);
  else newText(at);
}

/** Abre el editor sobre un fragmento de texto original del PDF (experimental). */
function editOriginalText(hit: TextObjectHit) {
  const index = state.current;
  const fontPx = hit.size * pxPerPt();
  const a = toPx(hit.rect.from);
  const b = toPx(hit.rect.to);
  // El rectángulo empieza en lo alto de las letras; el cuadro de texto, un poco más arriba.
  const top = { x: hit.rect.from.x, y: hit.rect.from.y - (fontPx * 0.15) / pageHeightPx() };
  openTextEditor({
    at: top,
    value: hit.text,
    fontPx,
    color: hit.color,
    fontFamily: cssFont(hit.font),
    minWidth: b.x - a.x,
    opaque: true,
    singleLine: true,
    onCommit: async (text) => {
      const result = await edit<TextEditResult>(
        "edit_text_object",
        { index, objects: hit.objects, text },
        index,
      );
      if (result?.fontReplaced) {
        showInfo("La fuente original no tiene todas esas letras: este fragmento ahora usa Helvetica.");
      }
    },
  });
}

// ---------- Selección y movimiento ----------

/** Algo que se puede tocar en la página: una anotación o un fragmento de texto original. */
type Hit = { kind: "annotation"; hit: AnnotationHit } | { kind: "object"; hit: TextObjectHit };

/**
 * Con Seleccionar se buscan primero anotaciones y luego texto original;
 * con Editar texto del PDF, solo texto original.
 */
async function hitAt(index: number, at: RelPoint, tool: Tool): Promise<Hit | null> {
  if (tool === "select") {
    const annotation = await invoke<AnnotationHit | null>("annotation_at", { index, at });
    if (annotation) return { kind: "annotation", hit: annotation };
  }
  const object = await invoke<TextObjectHit | null>("text_object_at", { index, at });
  return object ? { kind: "object", hit: object } : null;
}

function isMovable(h: Hit) {
  return h.kind === "object" || h.hit.text !== null;
}

function hitKey(h: Hit) {
  return h.kind === "object" ? `o${h.hit.objects.join(",")}` : `a${h.hit.annotation}`;
}

const NO_TEXT_HERE =
  "No hay texto editable en ese punto. Si ahí se ve texto, es parte de una imagen y no se puede editar como texto.";

let selection: { target: Hit; box: SVGRectElement } | null = null;
let lastClick = { key: "", time: 0 };

function clearSelection() {
  selection?.box.remove();
  selection = null;
}

/**
 * Clic: selecciona. Arrastrar: mueve un texto (propio u original).
 * Con Seleccionar, doble clic edita; con Editar texto del PDF, basta un clic.
 */
function pickAt(down: PointerEvent, tool: Tool) {
  const index = state.current;
  const start = relPoint(down);
  const r = els.page.getBoundingClientRect();
  let end = start;

  const hitPromise = hitAt(index, start, tool).catch((e) => (showError(e), null));
  hitPromise.then((target) => {
    clearSelection();
    if (!target || index !== state.current) return;
    selection = { target, box: outline(target.hit.rect, "selection") };
  });

  trackPointer(
    down,
    (e) => {
      end = relPoint(e);
      if (selection && isMovable(selection.target)) {
        const dx = (end.x - start.x) * r.width;
        const dy = (end.y - start.y) * r.height;
        selection.box.setAttribute("transform", `translate(${dx} ${dy})`);
      }
    },
    async () => {
      const target = await hitPromise;
      if (index !== state.current) return;
      if (!target) {
        if (tool === "edit-text") showInfo(NO_TEXT_HERE);
        return;
      }
      const moved = Math.hypot((end.x - start.x) * r.width, (end.y - start.y) * r.height) >= 3;

      if (moved && isMovable(target)) {
        lastClick.key = "";
        await moveTarget(index, target, start, end);
        return;
      }

      const now = performance.now();
      const double = lastClick.key === hitKey(target) && now - lastClick.time < 400;
      lastClick = { key: hitKey(target), time: now };
      if (tool === "edit-text" || double) {
        if (target.kind === "object") editOriginalText(target.hit);
        else if (target.hit.text) editOwnText(target.hit.annotation, target.hit.text);
      }
    },
  );
}

function moveTarget(index: number, target: Hit, from: RelPoint, to: RelPoint) {
  if (target.kind === "object") {
    return edit("move_text_object", { index, objects: target.hit.objects, from, to }, index);
  }
  const info = target.hit.text!;
  const topLeft = topLeftOf(info);
  const at = { x: topLeft.x + to.x - from.x, y: topLeft.y + to.y - from.y };
  const lines = layoutLines(info.lines.join("\n"), at, info.size * pxPerPt());
  return edit(
    "update_text",
    { index, annotation: target.hit.annotation, lines, size: info.size, color: info.color },
    index,
  );
}

function deleteSelection() {
  if (!selection) return;
  const { target } = selection;
  const index = state.current;
  clearSelection();
  if (target.kind === "annotation") {
    edit("delete_annotation", { index, annotation: target.hit.annotation }, index);
  } else {
    // Vaciar un fragmento original lo elimina.
    edit("edit_text_object", { index, objects: target.hit.objects, text: "" }, index);
  }
}

// ---------- Resaltado al pasar el puntero ----------

let hoverBox: SVGRectElement | null = null;
let hoverPending: RelPoint | null = null;
let hoverRunning = false;

function clearHover() {
  hoverBox?.remove();
  hoverBox = null;
}

/** Marca lo que se puede tocar bajo el puntero. Solo hay una consulta en curso a la vez. */
async function onOverlayHover(e: PointerEvent) {
  if (!state.doc || state.busy || textInput || e.buttons) return;
  if (state.tool !== "select" && state.tool !== "edit-text" && state.tool !== "batch") return;
  hoverPending = relPoint(e);
  if (hoverRunning) return;
  hoverRunning = true;
  try {
    while (hoverPending) {
      const at = hoverPending;
      hoverPending = null;
      const index = state.current;
      const target = await hitAt(index, at, state.tool);
      clearHover();
      els.overlay.style.cursor = !target ? "" : isMovable(target) ? "move" : "pointer";
      if (target && index === state.current && !textInput && !state.busy) {
        hoverBox = outline(target.hit.rect, "hover");
      }
    }
  } catch {
    // Una consulta fallida solo deja de marcar.
  } finally {
    hoverRunning = false;
  }
}

function startShape(down: PointerEvent, tool: "highlight" | "rect") {
  const start = relPoint(down);
  const color = state.colors[tool];
  const shape = svg("rect", {
    fill: tool === "highlight" ? color : "none",
    "fill-opacity": tool === "highlight" ? 0.4 : 0,
    stroke: tool === "rect" ? color : "none",
    "stroke-width": state.strokeWidth * pxPerPt(),
  });
  els.overlay.append(shape);
  let end = start;

  const draw = () => {
    const a = toPx(start);
    const b = toPx(end);
    shape.setAttribute("x", String(Math.min(a.x, b.x)));
    shape.setAttribute("y", String(Math.min(a.y, b.y)));
    shape.setAttribute("width", String(Math.abs(a.x - b.x)));
    shape.setAttribute("height", String(Math.abs(a.y - b.y)));
  };

  trackPointer(
    down,
    (e) => {
      end = relPoint(e);
      draw();
    },
    async () => {
      const a = toPx(start);
      const b = toPx(end);
      if (Math.abs(a.x - b.x) < 3 || Math.abs(a.y - b.y) < 3) return shape.remove();
      const index = state.current;
      const args =
        tool === "highlight"
          ? { index, from: start, to: end, color }
          : { index, from: start, to: end, color, width: state.strokeWidth };
      if (!(await edit(tool === "highlight" ? "add_highlight" : "add_rect", args, index))) {
        shape.remove();
      }
    },
  );
}

function startPen(down: PointerEvent) {
  const points = [relPoint(down)];
  const line = svg("polyline", {
    fill: "none",
    stroke: state.colors.pen,
    "stroke-width": state.strokeWidth * pxPerPt(),
    "stroke-linecap": "round",
    "stroke-linejoin": "round",
  });
  els.overlay.append(line);

  const draw = () => {
    line.setAttribute(
      "points",
      points
        .map(toPx)
        .map((p) => `${p.x},${p.y}`)
        .join(" "),
    );
  };
  draw();

  trackPointer(
    down,
    (e) => {
      const p = relPoint(e);
      const last = toPx(points[points.length - 1]);
      const cur = toPx(p);
      if (Math.hypot(cur.x - last.x, cur.y - last.y) >= 1.5) {
        points.push(p);
        draw();
      }
    },
    async () => {
      const index = state.current;
      const args = { index, points, width: state.strokeWidth, color: state.colors.pen };
      if (!(await edit("add_ink", args, index))) line.remove();
    },
  );
}

function trackPointer(down: PointerEvent, onMove: (e: PointerEvent) => void, onUp: () => void) {
  els.overlay.setPointerCapture(down.pointerId);
  const move = (e: PointerEvent) => onMove(e);
  els.overlay.addEventListener("pointermove", move);
  els.overlay.addEventListener(
    "pointerup",
    () => {
      els.overlay.removeEventListener("pointermove", move);
      onUp();
    },
    { once: true },
  );
}

function onOverlayPointerDown(e: PointerEvent) {
  if (e.button !== 0 || !state.doc || state.busy) return;
  e.preventDefault();
  // Con un texto en edición, el clic fuera solo lo confirma.
  if (textInput) {
    finishTextInput();
    return;
  }
  clearHover();
  switch (state.tool) {
    case "select":
    case "edit-text":
      pickAt(e, state.tool);
      break;
    case "batch":
      toggleBatchField(relPoint(e));
      break;
    case "text":
      onTextToolClick(relPoint(e));
      break;
    case "highlight":
    case "rect":
      startShape(e, state.tool);
      break;
    case "pen":
      startPen(e);
      break;
    case "eraser":
      edit("erase_at", { index: state.current, at: relPoint(e) }, state.current);
      break;
  }
}

// ---------- Generación en serie ----------

interface BatchField {
  page: number;
  objects: number[];
  text: string;
  rect: RelRect;
  column: number;
  align: "left" | "center" | "right";
}

interface Table {
  headers: string[];
  rows: string[][];
}

interface BatchResult {
  generated: number;
  cancelled: boolean;
  fontReplacedRows: number[];
  files: string[];
}

const ALIGN_LABELS = { left: "Izq.", center: "Centro", right: "Der." } as const;

const batch = {
  active: false,
  running: false,
  fields: [] as BatchField[],
  table: null as Table | null,
  tableName: "",
  /** Fila que se muestra en la vista previa. */
  row: 0,
  /** El nombre de archivo se completa solo hasta que la persona lo edita. */
  patternAuto: true,
};

function batchPreviewing() {
  return batch.active && batch.fields.length > 0 && (batch.table?.rows.length ?? 0) > 0;
}

function backendFields() {
  return batch.fields.map(({ page, objects, column, align }) => ({ page, objects, column, align }));
}

async function renderBatchPreview(index: number, width: number) {
  const buf = await invoke<ArrayBuffer>("batch_preview", {
    fields: backendFields(),
    row: batch.table!.rows[batch.row],
    page: index,
    width: Math.round(width),
  });
  return URL.createObjectURL(new Blob([buf], { type: "image/png" }));
}

function openBatch() {
  if (!state.doc || batch.active) return;
  finishTextInput();
  clearSelection();
  batch.active = true;
  setTool("batch");
  els.batchPanel.hidden = false;
  els.batchResult.textContent = "";
  renderBatchPanel();
  updateUi();
}

function closeBatch() {
  if (!batch.active || batch.running) return;
  Object.assign(batch, { active: false, fields: [], table: null, tableName: "", row: 0, patternAuto: true });
  els.batchPattern.value = "";
  els.batchPanel.hidden = true;
  setTool("select");
  updateUi();
  renderCurrent().catch(showError);
}

/** Dibuja los campos marcados de la página actual con su número. */
function drawBatchFields() {
  if (!batch.active) return;
  batch.fields.forEach((field, i) => {
    if (field.page !== state.current) return;
    outline(field.rect, "field");
    const corner = toPx(field.rect.from);
    const label = svg("text", { class: "field-label", x: corner.x - 2, y: corner.y - 6 });
    label.textContent = String(i + 1);
    els.overlay.append(label);
  });
}

async function toggleBatchField(at: RelPoint) {
  const index = state.current;
  const hit = await invoke<TextObjectHit | null>("text_object_at", { index, at }).catch(
    (e) => (showError(e), null),
  );
  if (index !== state.current || !batch.active) return;
  if (!hit) return showInfo(NO_TEXT_HERE);

  const key = hit.objects.join(",");
  const existing = batch.fields.findIndex((f) => f.page === index && f.objects.join(",") === key);
  if (existing >= 0) {
    batch.fields.splice(existing, 1);
  } else {
    // Si el texto está centrado en la página, lo más probable es que deba seguir centrado.
    const center = (hit.rect.from.x + hit.rect.to.x) / 2;
    const columns = batch.table?.headers.length ?? Infinity;
    batch.fields.push({
      page: index,
      objects: hit.objects,
      text: hit.text,
      rect: hit.rect,
      column: Math.min(batch.fields.length, columns - 1),
      align: Math.abs(center - 0.5) < 0.04 ? "center" : "left",
    });
  }
  onBatchChanged();
}

function onBatchChanged() {
  renderBatchPanel();
  renderCurrent().catch(showError);
}

function fileNames() {
  const { headers, rows } = batch.table!;
  return rows.map((row) =>
    els.batchPattern.value.replace(/\{([^}]*)\}/g, (_, name: string) => {
      const i = headers.findIndex((h) => h.toLowerCase() === name.trim().toLowerCase());
      return i >= 0 ? row[i] : "";
    }),
  );
}

function batchMode() {
  return document.querySelector<HTMLInputElement>('input[name="batch-mode"]:checked')!.value as
    | "files"
    | "single";
}

function renderBatchPanel() {
  const table = batch.table;

  els.batchFields.replaceChildren(
    ...batch.fields.map((field, i) => {
      const li = document.createElement("li");
      const text = document.createElement("div");
      text.className = "field-text";
      text.title = field.text;
      text.textContent = `${i + 1}. “${field.text}” `;
      const page = document.createElement("small");
      page.textContent = `pág. ${field.page + 1}`;
      text.append(page);

      const remove = document.createElement("button");
      remove.className = "icon";
      remove.title = "Quitar campo";
      remove.textContent = "×";
      remove.addEventListener("click", () => {
        batch.fields.splice(i, 1);
        onBatchChanged();
      });

      const options = document.createElement("div");
      options.className = "field-options";
      const column = document.createElement("select");
      column.title = "Columna con los valores";
      if (table) {
        table.headers.forEach((h, c) => column.append(new Option(h, String(c), false, c === field.column)));
      } else {
        column.append(new Option("Carga los datos…", ""));
        column.disabled = true;
      }
      column.addEventListener("change", () => {
        field.column = Number(column.value);
        onBatchChanged();
      });

      const align = document.createElement("div");
      align.className = "group segmented small";
      align.title = "Alineación del texto nuevo";
      for (const value of ["left", "center", "right"] as const) {
        const b = document.createElement("button");
        b.textContent = ALIGN_LABELS[value];
        b.setAttribute("aria-pressed", String(field.align === value));
        b.addEventListener("click", () => {
          field.align = value;
          onBatchChanged();
        });
        align.append(b);
      }
      options.append(column, align);
      li.append(text, remove, options);
      return li;
    }),
  );

  els.batchTableInfo.textContent = table
    ? `${batch.tableName}: ${table.rows.length} filas · columnas: ${table.headers.join(", ")}`
    : "La primera fila debe tener los nombres de las columnas.";

  const previewing = batchPreviewing();
  els.batchPreviewNav.hidden = !previewing;
  if (previewing) {
    const value = table!.rows[batch.row][batch.fields[0].column] ?? "";
    els.batchRowLabel.textContent = `Vista previa: fila ${batch.row + 1} de ${table!.rows.length} · ${value}`;
    els.batchPrev.disabled = batch.row === 0;
    els.batchNext.disabled = batch.row >= table!.rows.length - 1;
  }

  if (batch.patternAuto && table && batch.fields.length) {
    els.batchPattern.value = `{${table.headers[batch.fields[0].column]}}`;
  }
  els.batchPattern.disabled = batchMode() !== "files";

  const count = table?.rows.length ?? 0;
  els.batchRun.disabled = !previewing || batch.running;
  els.batchRun.textContent =
    batchMode() === "files"
      ? `Generar ${count || ""} PDF${count === 1 ? "" : "s"}`
      : `Generar 1 PDF con ${count || ""} página${count === 1 ? "" : "s"}`;
}

async function loadBatchTable() {
  const path = await open({
    multiple: false,
    filters: [{ name: "Excel o CSV", extensions: ["xlsx", "xlsm", "xls", "ods", "csv", "tsv", "txt"] }],
  });
  if (!path) return;
  try {
    const table = await invoke<Table>("read_table_file", { path });
    batch.table = table;
    batch.tableName = baseName(path);
    batch.row = 0;
    batch.fields.forEach((f, i) => (f.column = Math.min(i, table.headers.length - 1)));
    onBatchChanged();
  } catch (e) {
    showError(e);
  }
}

async function runBatch() {
  if (!batchPreviewing() || batch.running) return;
  const table = batch.table!;
  let output: Record<string, unknown>;
  if (batchMode() === "files") {
    const dir = await open({ directory: true, title: "Carpeta donde guardar los PDFs" });
    if (!dir) return;
    output = { mode: "files", dir, names: fileNames() };
  } else {
    const path = await save({ defaultPath: "generados.pdf", filters: PDF_FILTER });
    if (!path) return;
    output = { mode: "single", path };
  }

  batch.running = true;
  els.batchResult.textContent = "";
  els.batchProgress.hidden = false;
  els.batchProgressBar.value = 0;
  els.batchProgressBar.max = table.rows.length;
  els.batchProgressLabel.textContent = `0 / ${table.rows.length}`;
  renderBatchPanel();
  const unlisten = await listen<{ done: number; total: number }>("batch-progress", (e) => {
    els.batchProgressBar.value = e.payload.done;
    els.batchProgressLabel.textContent = `${e.payload.done} / ${e.payload.total}`;
  });

  try {
    const result = await invoke<BatchResult>("batch_generate", {
      fields: backendFields(),
      rows: table.rows,
      output,
    });
    const where =
      output.mode === "files" ? `en ${baseName(String(output.dir))}` : `en ${baseName(String(output.path))}`;
    let message = result.cancelled
      ? output.mode === "files"
        ? `Cancelado: se generaron ${result.generated} de ${table.rows.length} PDFs ${where}.`
        : "Cancelado: no se guardó el archivo."
      : output.mode === "files"
        ? `Listo: ${result.generated} PDFs ${where}.`
        : `Listo: ${result.generated} páginas ${where}.`;
    const replaced = result.fontReplacedRows;
    if (replaced.length) {
      const list = replaced.slice(0, 8).map((r) => r + 1).join(", ") + (replaced.length > 8 ? "…" : "");
      message += ` En ${replaced.length} fila${replaced.length === 1 ? "" : "s"} (${list}) la fuente original no tenía todas las letras y se usó Helvetica.`;
    }
    els.batchResult.textContent = message;
  } catch (e) {
    showError(e);
  } finally {
    unlisten();
    batch.running = false;
    els.batchProgress.hidden = true;
    renderBatchPanel();
  }
}

// ---------- Eventos ----------

function isTyping(e: KeyboardEvent) {
  const t = e.target as HTMLElement;
  return t.tagName === "TEXTAREA" || (t.tagName === "INPUT" && (t as HTMLInputElement).type === "text");
}

const TOOL_KEYS: Record<string, Tool> = {
  v: "select",
  t: "text",
  x: "edit-text",
  h: "highlight",
  r: "rect",
  p: "pen",
  e: "eraser",
};

window.addEventListener("DOMContentLoaded", () => {
  els.open.addEventListener("click", openDocument);
  els.openEmpty.addEventListener("click", openDocument);
  els.save.addEventListener("click", () => saveDocument(false));
  els.saveAs.addEventListener("click", () => saveDocument(true));
  els.undo.addEventListener("click", () => history("undo"));
  els.redo.addEventListener("click", () => history("redo"));
  els.prev.addEventListener("click", () => goTo(state.current - 1));
  els.next.addEventListener("click", () => goTo(state.current + 1));
  els.zoomOut.addEventListener("click", () => zoom(-1));
  els.zoomIn.addEventListener("click", () => zoom(1));
  els.rotateLeft.addEventListener("click", () =>
    edit("rotate_page", { index: state.current, clockwise: false }),
  );
  els.rotateRight.addEventListener("click", () =>
    edit("rotate_page", { index: state.current, clockwise: true }),
  );
  els.blank.addEventListener("click", insertBlankPage);
  els.insert.addEventListener("click", insertDocument);
  els.del.addEventListener("click", () => edit("delete_page", { index: state.current }));

  els.tools.addEventListener("click", (e) => {
    const tool = (e.target as HTMLElement).closest<HTMLButtonElement>("button")?.dataset.tool;
    if (tool) setTool(tool as Tool);
  });
  els.color.addEventListener("input", () => {
    if (state.tool in state.colors) state.colors[state.tool] = els.color.value;
    if (textInput?.dataset.styled !== undefined) textInput.style.color = els.color.value;
  });
  els.color.addEventListener("change", () => textInput?.focus());
  els.size.addEventListener("change", () => {
    state.textSize = Number(els.size.value);
    if (textInput?.dataset.styled !== undefined) {
      textInput.style.fontSize = `${state.textSize * pxPerPt()}px`;
      autosize(textInput);
      textInput.focus();
    }
  });
  els.width.addEventListener("input", () => (state.strokeWidth = Number(els.width.value)));
  els.overlay.addEventListener("pointerdown", onOverlayPointerDown);
  els.batch.addEventListener("click", openBatch);
  els.batchClose.addEventListener("click", closeBatch);
  els.batchLoad.addEventListener("click", loadBatchTable);
  els.batchRun.addEventListener("click", runBatch);
  els.batchCancel.addEventListener("click", () => invoke("batch_cancel"));
  els.batchPrev.addEventListener("click", () => {
    batch.row = Math.max(0, batch.row - 1);
    onBatchChanged();
  });
  els.batchNext.addEventListener("click", () => {
    batch.row = Math.min((batch.table?.rows.length ?? 1) - 1, batch.row + 1);
    onBatchChanged();
  });
  els.batchPattern.addEventListener("input", () => {
    batch.patternAuto = els.batchPattern.value === "";
  });
  document.querySelectorAll('input[name="batch-mode"]').forEach((r) =>
    r.addEventListener("change", renderBatchPanel),
  );
  els.overlay.addEventListener("pointermove", onOverlayHover);
  els.overlay.addEventListener("pointerleave", () => {
    hoverPending = null;
    clearHover();
  });

  window.addEventListener("keydown", (e) => {
    if (isTyping(e)) return;
    const mod = e.metaKey || e.ctrlKey;
    const key = e.key.toLowerCase();
    if (mod && key === "o") {
      e.preventDefault();
      openDocument();
    } else if (mod && key === "s") {
      e.preventDefault();
      saveDocument(e.shiftKey);
    } else if (mod && key === "z") {
      e.preventDefault();
      history(e.shiftKey ? "redo" : "undo");
    } else if (mod && key === "y") {
      e.preventDefault();
      history("redo");
    } else if (mod && (key === "=" || key === "+")) {
      e.preventDefault();
      zoom(1);
    } else if (mod && key === "-") {
      e.preventDefault();
      zoom(-1);
    } else if (!mod && (key === "arrowright" || key === "pagedown")) {
      goTo(state.current + 1);
    } else if (!mod && (key === "arrowleft" || key === "pageup")) {
      goTo(state.current - 1);
    } else if (batch.active) {
      if (!mod && key === "escape") closeBatch();
    } else if (!mod && (key === "delete" || key === "backspace") && selection) {
      e.preventDefault();
      deleteSelection();
    } else if (!mod && key === "escape") {
      if (selection) clearSelection();
      else setTool("select");
    } else if (!mod && !e.altKey && key in TOOL_KEYS) {
      setTool(TOOL_KEYS[key]);
    }
  });

  // Arrastrar PDFs a la ventana: el primero se abre si no hay documento, el resto se inserta.
  getCurrentWebview()
    .onDragDropEvent(async (event) => {
      if (event.payload.type !== "drop") return;
      const paths = event.payload.paths.filter(isPdf);
      if (!paths.length) return;
      if (!state.doc) await openPath(paths.shift()!);
      if (state.doc && paths.length) await insertPdfs(paths);
    })
    .catch(() => {});

  getCurrentWindow()
    .onCloseRequested(async (event) => {
      if (state.dirty && !(await confirmDiscard())) event.preventDefault();
    })
    .catch(() => {});

  updateUi();
});
