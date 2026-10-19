import { $, readStoredValue, writeStoredValue } from "./ui-utils.js";

const STORAGE_KEY = "doppeldecker-sidebar-section-order";
const SECTION_DRAG_TYPE = "application/x-doppeldecker-sidebar-section";
const DEFAULT_ORDER = ["devices", "shares", "favorites"];

/** Nur eine vollständige Permutation der bekannten Abschnitte darf die
 * DOM-Reihenfolge bestimmen. Fremde, doppelte oder alte Werte fallen auf den
 * festen Standard zurück, statt Teile der Seitenleiste verschwinden zu lassen. */
function validOrder(value) {
  return Array.isArray(value)
    && value.length === DEFAULT_ORDER.length
    && new Set(value).size === DEFAULT_ORDER.length
    && value.every(id => DEFAULT_ORDER.includes(id));
}

function savedOrder() {
  try {
    const value = JSON.parse(readStoredValue(STORAGE_KEY) || "null");
    return validOrder(value) ? value : DEFAULT_ORDER;
  } catch (_) {
    return DEFAULT_ORDER;
  }
}

/** Verdrahtet die drei sichtbaren Hauptkategorien. Die Reihenfolge ist reine
 * lokale Darstellung: Sie verändert weder Mounts noch Favoriten oder Profile. */
export function initSidebarSections() {
  const container = $("#sidebar-sections");
  const sections = new Map(
    [...container.querySelectorAll(":scope > [data-sidebar-section]")]
      .map(section => [section.dataset.sidebarSection, section]),
  );

  function currentOrder() {
    return [...container.querySelectorAll(":scope > [data-sidebar-section]")]
      .map(section => section.dataset.sidebarSection);
  }

  function apply(order, persist = false) {
    const safeOrder = validOrder(order) && order.every(id => sections.has(id))
      ? order
      : DEFAULT_ORDER;
    for (const id of safeOrder) container.append(sections.get(id));
    currentOrder().forEach((id, index) => {
      const handle = sections.get(id).querySelector(".side-section-handle");
      handle.setAttribute("aria-posinset", String(index + 1));
      handle.setAttribute("aria-setsize", String(DEFAULT_ORDER.length));
    });
    if (persist) {
      // Ohne localStorage gilt die neue Reihenfolge nur für dieses Fenster.
      writeStoredValue(STORAGE_KEY, JSON.stringify(safeOrder));
    }
  }

  function move(id, targetIndex) {
    const order = currentOrder();
    const from = order.indexOf(id);
    if (from < 0) return;
    order.splice(from, 1);
    order.splice(Math.max(0, Math.min(targetIndex, order.length)), 0, id);
    apply(order, true);
    sections.get(id).querySelector(".side-section-handle").focus();
  }

  function clearDragState() {
    for (const section of sections.values()) {
      section.classList.remove("dragging", "drop-before", "drop-after");
      section.querySelector(".side-section-handle").setAttribute("aria-grabbed", "false");
    }
  }

  apply(savedOrder());

  container.addEventListener("dragstart", event => {
    const handle = event.target.closest(".side-section-handle");
    if (!handle) return;
    const section = handle.closest("[data-sidebar-section]");
    const id = section?.dataset.sidebarSection;
    if (!sections.has(id)) return;
    event.dataTransfer.setData(SECTION_DRAG_TYPE, id);
    event.dataTransfer.effectAllowed = "move";
    section.classList.add("dragging");
    handle.setAttribute("aria-grabbed", "true");
  });

  container.addEventListener("dragover", event => {
    if (!event.dataTransfer?.types.includes(SECTION_DRAG_TYPE)) return;
    const target = event.target.closest("[data-sidebar-section]");
    if (!target) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
    for (const section of sections.values()) {
      section.classList.remove("drop-before", "drop-after");
    }
    const rect = target.getBoundingClientRect();
    target.classList.add(event.clientY > rect.top + rect.height / 2 ? "drop-after" : "drop-before");
  });

  container.addEventListener("dragleave", event => {
    if (!container.contains(event.relatedTarget)) clearDragState();
  });

  container.addEventListener("drop", event => {
    const id = event.dataTransfer?.getData(SECTION_DRAG_TYPE);
    const target = event.target.closest("[data-sidebar-section]");
    if (!sections.has(id) || !target) return;
    event.preventDefault();
    const order = currentOrder();
    const from = order.indexOf(id);
    let insertion = order.indexOf(target.dataset.sidebarSection);
    const rect = target.getBoundingClientRect();
    if (event.clientY > rect.top + rect.height / 2) insertion += 1;
    if (from < insertion) insertion -= 1;
    clearDragState();
    move(id, insertion);
  });

  container.addEventListener("dragend", clearDragState);

  container.addEventListener("keydown", event => {
    const handle = event.target.closest(".side-section-handle");
    if (!handle) return;
    const id = handle.closest("[data-sidebar-section]")?.dataset.sidebarSection;
    const order = currentOrder();
    const index = order.indexOf(id);
    if (index < 0) return;
    let target = null;
    if (event.key === "ArrowUp") target = index - 1;
    else if (event.key === "ArrowDown") target = index + 1;
    else if (event.key === "Home") target = 0;
    else if (event.key === "End") target = order.length - 1;
    if (target === null) return;
    event.preventDefault();
    move(id, target);
  });
}
