import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
import elkLayouts from "https://cdn.jsdelivr.net/npm/@mermaid-js/layout-elk@0/dist/mermaid-layout-elk.esm.min.mjs";

mermaid.registerLayoutLoaders(elkLayouts);

const sources = __SOURCES__;
const storageKey = "supervisor-mermaid:appearance";
const modes = new Set(["system", "light", "dark"]);
const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
const canvases = Array.from(document.querySelectorAll(".diagram-canvas"));
const buttons = Array.from(document.querySelectorAll("[data-theme-mode]"));
let mode = readStoredMode();
let renderGeneration = 0;

function readStoredMode() {
  try {
    const stored = window.localStorage.getItem(storageKey);
    return modes.has(stored) ? stored : "system";
  } catch {
    return "system";
  }
}

function resolvedTheme() {
  return mode === "dark" || (mode === "system" && mediaQuery.matches) ? "dark" : "default";
}

function updatePageTheme() {
  document.documentElement.dataset.theme = resolvedTheme() === "dark" ? "dark" : "light";
  for (const button of buttons) {
    button.setAttribute("aria-pressed", String(button.dataset.themeMode === mode));
  }
}

function persistMode() {
  try {
    window.localStorage.setItem(storageKey, mode);
  } catch {
  }
}

async function renderDiagrams() {
  const generation = ++renderGeneration;
  mermaid.initialize({ startOnLoad: false, securityLevel: "loose", theme: resolvedTheme() });
  for (const [index, canvas] of canvases.entries()) {
    canvas.setAttribute("aria-busy", "true");
    try {
      const { svg, bindFunctions } = await mermaid.render(
        `supervisor-mermaid-${generation}-${index}`,
        sources[index],
      );
      if (generation !== renderGeneration) {
        return;
      }
      canvas.innerHTML = svg;
      bindFunctions?.(canvas);
    } catch (error) {
      if (generation !== renderGeneration) {
        return;
      }
      canvas.textContent = "Unable to render this diagram.";
      console.error("supervisor-mermaid:", error);
    } finally {
      if (generation === renderGeneration) {
        canvas.removeAttribute("aria-busy");
      }
    }
  }
}

function setMode(next, persist) {
  mode = next;
  if (persist) {
    persistMode();
  }
  updatePageTheme();
  void renderDiagrams();
}

for (const button of buttons) {
  button.addEventListener("click", () => setMode(button.dataset.themeMode, true));
}
const syncSystemTheme = () => {
  if (mode === "system") {
    updatePageTheme();
    void renderDiagrams();
  }
};
if (mediaQuery.addEventListener) {
  mediaQuery.addEventListener("change", syncSystemTheme);
} else {
  mediaQuery.addListener(syncSystemTheme);
}
setMode(mode, false);