export function createPaneTabsView({ side, paneEl, select, close, add, locked, profileName }) {
  const bar = paneEl.querySelector(".pane-tabs");
  const panel = paneEl.querySelector(".pane-content");
  const tabId = id => `pane-tab-${side}-${id}`;
  const label = tab => {
    const path = tab.location.path;
    const name = path.split("/").filter(Boolean).at(-1) || "/";
    return tab.location.provider === "local" ? name
      : `${profileName(tab.location) || tab.location.provider.toUpperCase()} · ${name}`;
  };
  function focus(id) { document.getElementById(tabId(id))?.focus(); }
  function render(model) {
    const focused = bar.contains(document.activeElement);
    const wasAdd = document.activeElement?.classList.contains("pane-tab-add");
    bar.replaceChildren();
    for (const tab of model.tabs) {
      const selected = tab.id === model.active.id;
      const group = document.createElement("div");
      group.className = "pane-tab";
      group.classList.toggle("selected", selected);
      const button = document.createElement("button");
      button.type = "button";
      button.className = "pane-tab-select";
      button.id = tabId(tab.id);
      button.dataset.tabId = tab.id;
      button.setAttribute("role", "tab");
      button.setAttribute("aria-selected", String(selected));
      button.setAttribute("aria-controls", panel.id);
      button.tabIndex = selected ? 0 : -1;
      button.title = tab.location.path;
      if (tab.runtime.status === "failed") button.title += " · Laden fehlgeschlagen";
      button.setAttribute("aria-busy", String(tab.runtime.status === "loading"));
      button.textContent = label(tab);
      button.disabled = locked();
      button.addEventListener("click", () => select(tab.id));
      button.addEventListener("keydown", event => {
        const tabs = model.tabs;
        const index = tabs.findIndex(item => item.id === tab.id);
        let next;
        if (event.key === "ArrowRight") next = tabs[(index + 1) % tabs.length];
        else if (event.key === "ArrowLeft") next = tabs[(index + tabs.length - 1) % tabs.length];
        else if (event.key === "Home") next = tabs[0];
        else if (event.key === "End") next = tabs.at(-1);
        else if (event.key === "Delete" || event.key === "Backspace") close(tab.id);
        else return;
        event.preventDefault();
        if (next) { select(next.id); focus(next.id); }
      });
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "pane-tab-close";
      remove.textContent = "×";
      remove.setAttribute("aria-label", `Tab ${label(tab)} schließen`);
      remove.title = "Tab schließen (⌘⇧W / Ctrl+Shift+W)";
      remove.disabled = locked() || model.tabs.length === 1;
      remove.dataset.tabUnavailable = String(model.tabs.length === 1);
      remove.addEventListener("click", () => close(tab.id));
      group.append(button, remove);
      bar.append(group);
      if (selected) panel.setAttribute("aria-labelledby", button.id);
    }
    const plus = document.createElement("button");
    plus.type = "button";
    plus.className = "pane-tab-add";
    plus.textContent = "+";
    plus.setAttribute("aria-label", "Neuer Tab");
    plus.title = "Neuer Tab (⌘T / Ctrl+T)";
    plus.disabled = locked() || model.tabs.length === 32;
    plus.dataset.tabUnavailable = String(model.tabs.length === 32);
    plus.addEventListener("click", add);
    bar.append(plus);
    if (focused) {
      if (wasAdd) plus.focus();
      else focus(model.active.id);
    }
    bar.querySelector(".pane-tab.selected")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }
  return { render, focus };
}
