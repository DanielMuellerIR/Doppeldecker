// Gemeinsame Dialogsteuerung. Sie bleibt unabhängig vom fachlichen Zustand, damit
// jeder Dialog dieselbe Tastatur-, Fokus- und Exklusivitätsgarantie verwendet.

import { $, $$ } from "./ui-utils.js";

let activeLease = null;
const pendingLeases = [];
let leaseSequence = 0;

/** Liefert nur tatsächlich bedienbare Elemente des sichtbaren Dialogs. Versteckte
 * Felder bleiben damit ebenso aus der Tab-Reihenfolge wie deaktivierte Knöpfe. */
function focusables() {
  return $$(
    '#modal .modal button:not([disabled]), #modal .modal input:not([disabled]):not([type="hidden"]), ' +
    '#modal .modal select:not([disabled]), #modal .modal a[href], #modal .modal [tabindex]:not([tabindex="-1"])',
  ).filter((element) => !element.hidden && element.getClientRects().length > 0);
}

function newLease(owner, render = null) {
  leaseSequence += 1;
  return {
    id: leaseSequence,
    owner,
    token: Symbol(`${owner}-${leaseSequence}`),
    render,
    presentation: null,
    returnFocus: null,
    parentLease: null,
  };
}

/** Bewahrt die echten Knoten statt nur HTML auf. So bleiben auch die zugehörigen
 * Klick- und Eingabe-Listener erhalten, wenn ein älterer Spezialdialog erst nach
 * einem bereits aufgebauten zweiten Dialog wieder eingesetzt werden muss. */
function capturePresentation(focusTarget, onEscape) {
  const modal = $("#modal .modal");
  const title = $("#modal-title");
  const body = $("#modal-body");
  const buttons = $("#modal .modal-buttons");
  return {
    modalClassName: modal.className,
    titleNodes: [...title.childNodes],
    bodyNodes: [...body.childNodes],
    buttonNodes: [...buttons.childNodes],
    bodyOnClick: body.onclick,
    bodyOnChange: body.onchange,
    focusTarget,
    onEscape,
  };
}

function restorePresentation(presentation) {
  const modal = $("#modal .modal");
  const body = $("#modal-body");
  modal.className = presentation.modalClassName;
  $("#modal-title").replaceChildren(...presentation.titleNodes);
  body.replaceChildren(...presentation.bodyNodes);
  body.onclick = presentation.bodyOnClick;
  body.onchange = presentation.bodyOnChange;
  $("#modal .modal-buttons").replaceChildren(...presentation.buttonNodes);
}

function focusActiveLease() {
  if (!activeLease) return;
  const requested = activeLease.presentation.focusTarget;
  const target = requested?.isConnected
    ? requested
    : focusables()[0] || $("#modal .modal");
  target.focus({ preventScroll: true });
}

function refreshPresentationMetadata(lease) {
  // Einige Spezialdialoge setzen delegierte Klickbehandlung oder ihre Breitenklasse
  // unmittelbar NACH `showModal()`. Am Ende desselben JavaScript-Schritts gehören
  // auch diese Angaben sicher zum Besitzer-Lease.
  queueMicrotask(() => {
    if (activeLease?.token !== lease.token) return;
    lease.presentation.modalClassName = $("#modal .modal").className;
    lease.presentation.bodyOnClick = $("#modal-body").onclick;
    lease.presentation.bodyOnChange = $("#modal-body").onchange;
  });
}

function resetPresentationShell() {
  $("#modal .modal").className = "modal";
  $("#modal-body").onclick = null;
  $("#modal-body").onchange = null;
}

function activateLease(lease) {
  lease.returnFocus = document.activeElement instanceof HTMLElement
    ? document.activeElement
    : null;
  if (lease.render) {
    resetPresentationShell();
    const setup = lease.render(lease.token);
    lease.presentation = capturePresentation(setup.focusTarget, setup.onEscape);
  } else {
    restorePresentation(lease.presentation);
  }
  activeLease = lease;
  $("#modal").hidden = false;
  focusActiveLease();
  refreshPresentationMetadata(lease);
}

function resumeParentLease(finished) {
  const parent = finished.parentLease;
  if (!parent) return false;
  restorePresentation(parent.presentation);
  activeLease = parent;
  $("#modal").hidden = false;
  const target = finished.returnFocus;
  if (target?.isConnected) target.focus({ preventScroll: true });
  else focusActiveLease();
  return true;
}

function activateNextLease() {
  if (activeLease || pendingLeases.length === 0) return;
  activateLease(pendingLeases.shift());
}

function enqueueRenderedModal(owner, render, { parentToken = null } = {}) {
  const lease = newLease(owner, render);
  if (parentToken !== null) {
    // Ein Kinddialog darf seinen ausdrücklich benannten Eltern-Lease pausieren.
    // Ein veraltetes Token wird dagegen nie als normaler Dialog eingereiht: Sonst
    // könnte eine verspätete Antwort hinter einem fremden Besitzer auftauchen.
    if (activeLease?.token !== parentToken) return null;
    lease.parentLease = activeLease;
    activeLease = null;
    activateLease(lease);
    return lease.token;
  }
  if (activeLease || pendingLeases.length > 0) {
    pendingLeases.push(lease);
    if (!activeLease) setTimeout(activateNextLease, 0);
  } else {
    activateLease(lease);
  }
  return lease.token;
}

/** Baut einen Spezialdialog erst dann auf, wenn sein Lease wirklich aktiv ist.
 * Dadurch kann kein eingereihter Dialog die Knoten oder Listener des Besitzers
 * verändern, der gerade sichtbar ist. */
export function showOwnedModal(owner, render) {
  return enqueueRenderedModal(owner, render);
}

/** Ein fachlich wiederholbarer Hinweis (derzeit der native Exit) ersetzt seinen
 * eigenen wartenden oder sichtbaren Stand. Andere Besitzer bleiben unverändert;
 * insbesondere wird kein Quarantäne-Promise verdrängt. */
export function showLatestOwnedModal(owner, render) {
  if (activeLease?.owner === owner) {
    activeLease.render = render;
    resetPresentationShell();
    const setup = render(activeLease.token) || {};
    activeLease.presentation = capturePresentation(setup.focusTarget, setup.onEscape);
    focusActiveLease();
    refreshPresentationMetadata(activeLease);
    return activeLease.token;
  }
  const pending = pendingLeases.find(lease => lease.owner === owner);
  if (pending) {
    pending.render = render;
    return pending.token;
  }
  return enqueueRenderedModal(owner, render);
}

/** Entfernt einen noch wartenden Besitzer oder schließt exakt seinen aktiven
 * Lease. Diese Funktion ist bewusst tokenpflichtig. */
export function cancelOwnedModal(token) {
  if (!token) return false;
  if (activeLease?.token === token) return hideModal(token);
  const index = pendingLeases.findIndex(lease => lease.token === token);
  if (index < 0) return false;
  pendingLeases.splice(index, 1);
  return true;
}

/** Ersetzt nur den Inhalt des aktuell benannten Besitzers. Diese Prüfung bindet
 * auch verspätete asynchrone Antworten an genau den Dialog, der sie ausgelöst hat. */
export function replaceModal(token, render) {
  if (!token || activeLease?.token !== token) return false;
  resetPresentationShell();
  const setup = render(token) || {};
  activeLease.presentation = capturePresentation(setup.focusTarget, setup.onEscape);
  focusActiveLease();
  refreshPresentationMetadata(activeLease);
  return true;
}

/** Ist der gemeinsame Dialog gerade belegt? Auch ein bereits eingereihter Dialog
 * zählt dazu, damit asynchrone Aufrufer keinen dritten Dialog dazwischenschieben. */
export function isModalOpen() {
  return activeLease !== null || pendingLeases.length > 0;
}

/** Öffnet einen bereits aufgebauten Spezialdialog unter einem eigenen Besitzer-
 * Lease. Falls ein Dialog aktiv ist, wird der neue Aufbau als zweiter Knotenbestand
 * eingereiht und der aktive Dialog sofort wiederhergestellt. */
export function showModal(focusTarget = null, onEscape = null, owner = "special") {
  const lease = newLease(owner);
  lease.presentation = capturePresentation(focusTarget, onEscape);
  if (!activeLease && pendingLeases.length === 0) {
    activateLease(lease);
    return lease.token;
  }

  if (!activeLease) {
    pendingLeases.push(lease);
    setTimeout(activateNextLease, 0);
    return lease.token;
  }

  // Der Spezialdialog wurde von älteren Aufrufern bereits vor `showModal()` in die
  // gemeinsame Hülle geschrieben. Der aktive Lease hält seine echten Knoten fest;
  // wir setzen sie zurück, bevor der Browser den verdrängenden Zwischenstand malt.
  restorePresentation(activeLease.presentation);
  pendingLeases.push(lease);
  focusActiveLease();
  return lease.token;
}

/** Schließt ausschließlich den angegebenen Besitzer. Ein fehlendes oder veraltetes
 * Token ist immer wirkungslos und kann daher keinen späteren Besitzer schließen. */
export function hideModal(token) {
  if (!token || !activeLease || activeLease.token !== token) return false;
  const finished = activeLease;
  activeLease = null;
  if (resumeParentLease(finished)) return true;
  $("#modal").hidden = true;
  const target = finished.returnFocus;
  if (target?.isConnected) target.focus({ preventScroll: true });
  // Der nächste unabhängige Dialog beginnt erst im nächsten Task. So läuft zuerst
  // die Promise-Fortsetzung des gerade bestätigten Konflikts vollständig durch.
  setTimeout(activateNextLease, 0);
  return true;
}

export function onModalKeyDown(event) {
  if (!activeLease) return;
  const escapeHandler = activeLease.presentation.onEscape;
  if (event.key === "Escape" && escapeHandler) {
    event.preventDefault();
    event.stopPropagation();
    escapeHandler();
    return;
  }
  if (event.key !== "Tab") return;
  const elements = focusables();
  if (!elements.length) {
    event.preventDefault();
    $("#modal .modal").focus();
    return;
  }
  const first = elements[0];
  const last = elements[elements.length - 1];
  // Ein asynchroner Dialogschritt kann den gerade fokussierten Knopf sperren oder
  // entfernen. Dann gehört der aktive Knoten nicht mehr zur Tab-Reihenfolge. Ohne
  // diese Prüfung würde der Browser hinter das Modal springen.
  if (!elements.includes(document.activeElement)) {
    event.preventDefault();
    (event.shiftKey ? last : first).focus();
  } else if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

/**
 * Setzt die Modalhülle auf den neutralen Zustand und füllt Titel und Inhalt.
 *
 * Klassen und delegierte Handler gehören dem jeweiligen Besitzer. Die neutrale
 * Hülle darf keine Breite oder Eingabebehandlung des vorherigen Dialogs erben.
 * Rückgabe ist die geleerte Knopfleiste.
 */
function resetModalShell(title, bodyHtml) {
  resetPresentationShell();
  $("#modal-title").textContent = title;
  $("#modal-body").innerHTML = bodyHtml;
  $("#modal-body").onclick = null;
  const bar = $("#modal .modal-buttons");
  bar.innerHTML = "";
  return bar;
}

/**
 * Erzeugt eine Abschlussfunktion, die genau einmal wirkt.
 *
 * Ein Modal lässt sich über Knopf, Escape und den Abbruch eines Elternmodals
 * gleichzeitig beenden. Ohne diese Schranke würde das Promise mehrfach aufgelöst
 * und ein zweiter Klick still verworfen, statt gar nicht erst zu wirken.
 */
function oneShotFinish(token, resolve) {
  let finished = false;
  return (value) => {
    if (finished || !hideModal(token)) return;
    finished = true;
    resolve(value);
  };
}

/** Modal mit frei definierbaren Knöpfen. `buttons`: [{label, value, cls}]. */
export function choiceModal(title, bodyHtml, buttons, { parentToken = null } = {}) {
  return new Promise((resolve) => {
    const token = enqueueRenderedModal("choice", token => {
      const bar = resetModalShell(title, bodyHtml);
      const finish = oneShotFinish(token, resolve);
      for (const button of buttons) {
        const element = document.createElement("button");
        element.className = "btn " + (button.cls || "ghost");
        element.textContent = button.label;
        element.addEventListener("click", () => finish(button.value));
        bar.appendChild(element);
      }
      return {
        focusTarget: bar.querySelector("button"),
        onEscape: () => finish(buttons[0]?.value ?? null),
      };
    }, { parentToken });
    if (token === null) resolve(buttons[0]?.value ?? null);
  });
}

/** Ja/Nein-Bestätigung, deren erste Aktion immer der sichere Abbruch ist. */
export function confirmModal(title, bodyHtml, options = {}) {
  return choiceModal(title, bodyHtml, [
    { label: "Abbrechen", value: false, cls: "ghost" },
    { label: "Fortfahren", value: true, cls: "danger" },
  ], options);
}

/**
 * Bestätigung eines unabhängigen Backups vor einer nicht umkehrbaren Wirkung.
 *
 * Der Wortlaut ist bewusst überall derselbe: Doppeldecker prüft dieses Backup
 * technisch nicht, und diese Einschränkung darf je nach Aufrufweg nicht anders
 * klingen.
 */
export function confirmIndependentBackup(options = {}) {
  return confirmModal(
    "Unabhängiges Backup bestätigen",
    "Ich bestätige ein unabhängiges Backup und eine jüngste erfolgreiche Restore-Probe. Doppeldecker hat dieses externe Backup nicht technisch geprüft.",
    options,
  );
}

/**
 * Wahl zwischen Abbruch, Queue und direkter Ausführung eines gebundenen Plans.
 *
 * Die erste Aktion ist immer der sichere Abbruch, „Direkt bestätigen“ trägt die
 * Gefahrmarkierung. Ergebnis: `"cancel"`, `"queue"` oder `"direct"`.
 */
export function executionChoiceModal(title, bodyHtml, options = {}) {
  return choiceModal(title, bodyHtml, [
    { label: "Abbrechen", value: "cancel", cls: "ghost" },
    { label: "In Queue", value: "queue", cls: "ghost" },
    { label: "Direkt bestätigen", value: "direct", cls: "danger" },
  ], options);
}

/** Texteingabe mit Enter zum Bestätigen und Escape über die zentrale Abbruchaktion. */
export function promptModal(title, initialValue, { parentToken = null, description = null, inputLabel = null, copyableDescription = false } = {}) {
  return new Promise((resolve) => {
    const token = enqueueRenderedModal("prompt", token => {
      const bar = resetModalShell(
        title,
        '<input id="modal-input" class="modal-input" spellcheck="false" autocomplete="off" />',
      );
      const input = $("#modal-input");
      if (description) {
        const text = document.createElement(copyableDescription ? "textarea" : "p");
        text.className = `modal-description modal-description-scroll${copyableDescription ? " listing-omissions-text" : ""}`;
        if (copyableDescription) {
          text.readOnly = true;
          text.value = description;
        } else {
          text.textContent = description;
          text.setAttribute("role", "region");
        }
        text.tabIndex = 0;
        text.setAttribute("aria-label", "Wirkung und Einschränkungen");
        input.before(text);
      }
      if (inputLabel) {
        const label = document.createElement("label");
        label.className = "modal-input-label";
        label.htmlFor = input.id;
        label.textContent = inputLabel;
        input.before(label);
      }
      input.value = initialValue || "";
      const finish = oneShotFinish(token, resolve);
      const cancel = document.createElement("button");
      cancel.className = "btn ghost";
      cancel.textContent = "Abbrechen";
      cancel.addEventListener("click", () => finish(null));
      const ok = document.createElement("button");
      ok.className = "btn primary";
      ok.textContent = "OK";
      ok.addEventListener("click", () => finish(input.value));
      bar.append(cancel, ok);
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          finish(input.value);
        }
      });
      queueMicrotask(() => {
        if (input.isConnected) input.select();
      });
      return { focusTarget: input, onEscape: () => finish(null) };
    }, { parentToken });
    if (token === null) resolve(null);
  });
}
