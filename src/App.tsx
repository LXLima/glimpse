import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyThemeVariables } from "./utils/theme";
import {
  Search24Regular,
  Apps20Regular,
  Settings20Regular,
  Document20Regular,
  Globe20Regular,
  FolderOpen20Regular,
  Calculator20Regular,
  Dismiss20Regular
} from "@fluentui/react-icons";
import "./App.css";

interface SearchResult {
  name: string;
  path: string;
  kind: "app" | "file" | "setting" | "web" | "math" | "kill";
  score: number;
  icon_base64?: string;
}

interface FullConfig {
  hotkey: string;
  theme: string;
  startup: boolean;
  search_engine: string;
}

/** Web search targets for the fallback row. IDs must match the backend
 * whitelist (`sanitize_engine` in main.rs) - update both when adding one. */
const SEARCH_ENGINES: Record<string, { label: string; url: string }> = {
  google:     { label: "Google",     url: "https://www.google.com/search?q=" },
  bing:       { label: "Bing",       url: "https://www.bing.com/search?q=" },
  duckduckgo: { label: "DuckDuckGo", url: "https://duckduckgo.com/?q=" },
  brave:      { label: "Brave",      url: "https://search.brave.com/search?q=" },
};

const KIND_ICONS: Record<string, React.ReactNode> = {
  app:       <Apps20Regular />,
  file:      <Document20Regular />,
  setting:   <Settings20Regular />,
  web:       <Globe20Regular />,
  math:      <Calculator20Regular />,
  kill:      <Dismiss20Regular />,
};

/** How often we ask the backend whether the index finished building. */
const INDEX_POLL_MS = 400;
const INDEX_POLL_MAX_TRIES = 50;

interface Group {
  kind: string;
  items: { item: SearchResult; idx: number }[];
}

/** Groups by kind (first-appearance order) and returns both the flattened
 * list (source of truth for keyboard selection) and the grouped view. */
function groupResults(items: SearchResult[]): { groups: Group[]; flat: SearchResult[] } {
  const byKind = new Map<string, SearchResult[]>();
  for (const item of items) {
    const bucket = byKind.get(item.kind);
    if (bucket) bucket.push(item);
    else byKind.set(item.kind, [item]);
  }

  const flat: SearchResult[] = [];
  const groups: Group[] = [];
  byKind.forEach((bucket, kind) => {
    const entries = bucket.map((item) => {
      const idx = flat.length;
      flat.push(item);
      return { item, idx };
    });
    groups.push({ kind, items: entries });
  });
  return { groups, flat };
}

export default function App() {
  const [query, setQuery]         = useState("");
  const [results, setResults]     = useState<SearchResult[]>([]);
  const [selected, setSelected]   = useState(0);
  const [loading, setLoading]     = useState(false);
  const [theme, setTheme]         = useState<string>("system");
  const [engine, setEngine]       = useState<string>("google");
  const [hotkey, setHotkey]       = useState("Ctrl+Space");
  const [indexReady, setIndexReady] = useState(false);
  const [toast, setToast]         = useState<string | null>(null);
  const [copiedIdx, setCopiedIdx] = useState<number | null>(null);

  const inputRef     = useRef<HTMLInputElement>(null);
  const debounceRef  = useRef<ReturnType<typeof setTimeout> | null>(null);
  const seqRef       = useRef(0);
  const toastTimer   = useRef<ReturnType<typeof setTimeout> | null>(null);
  const copiedTimer  = useRef<ReturnType<typeof setTimeout> | null>(null);
  const navTimer     = useRef<ReturnType<typeof setTimeout> | null>(null);
  const isKeyboardNav = useRef(false);

  // ---------------------------------------------------------------- focus --
  // The palette is a hidden window that is shown by a global hotkey. WebView2
  // does *not* restore DOM focus by itself when the window reappears, so the
  // search box has to be re-focused explicitly - otherwise the first keystrokes
  // land nowhere and it feels like "I have to click it before I can type".
  const focusInput = useCallback((select: boolean) => {
    const input = inputRef.current;
    if (!input) return;
    input.focus({ preventScroll: true });
    if (select) input.select();
  }, []);

  const refocusLater = useCallback(() => {
    // WebView2 sometimes only settles focus a few frames after activation.
    [80, 220, 450].forEach((delay) => {
      window.setTimeout(() => {
        if (document.activeElement !== inputRef.current) focusInput(false);
      }, delay);
    });
  }, [focusInput]);

  const handleWindowShown = useCallback(() => {
    // Showing is focus-only. The panel carries no open animation at all, so
    // there is nothing that can flash or restart mid-flight.
    focusInput(true);
    refocusLater();
  }, [focusInput, refocusLater]);

  const handleWindowFocused = useCallback(() => {
    if (document.activeElement !== inputRef.current) focusInput(false);
    refocusLater();
  }, [focusInput, refocusLater]);

  // --------------------------------------------------------------- config --
  useEffect(() => {
    let disposed = false;
    invoke<FullConfig>("get_full_config")
      .then((cfg) => {
        if (disposed) return;
        setHotkey(cfg.hotkey);
        setTheme(cfg.theme);
        setEngine(cfg.search_engine);
      })
      .catch(console.error);

    const unlisteners = [
      listen("window-shown", handleWindowShown),
      listen("window-focused", handleWindowFocused),
      listen<FullConfig>("config-changed", (event) => {
        setHotkey(event.payload.hotkey);
        setTheme(event.payload.theme);
        setEngine(event.payload.search_engine);
      }),
    ];

    // DOM-level fallback in case the native focus event arrives without one of
    // the custom events above.
    window.addEventListener("focus", handleWindowFocused);

    return () => {
      disposed = true;
      window.removeEventListener("focus", handleWindowFocused);
      unlisteners.forEach((p) => p.then((unlisten) => unlisten()).catch(() => undefined));
    };
  }, [handleWindowShown, handleWindowFocused]);

  useEffect(() => {
    applyThemeVariables(theme);
    if (theme === "system") {
      const media = window.matchMedia("(prefers-color-scheme: dark)");
      const listener = () => applyThemeVariables(theme);
      media.addEventListener("change", listener);
      return () => media.removeEventListener("change", listener);
    }
  }, [theme]);

  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      const container = document.querySelector(".container");
      if (container && !container.contains(e.target as Node)) {
        invoke("hide_window").catch(console.error);
      }
    };
    window.addEventListener("mousedown", handleClickOutside);
    return () => window.removeEventListener("mousedown", handleClickOutside);
  }, []);

  // ---------------------------------------------------------- index status --
  useEffect(() => {
    let disposed = false;
    let tries = 0;

    const poll = async () => {
      try {
        const count = await invoke<number>("get_index_status");
        if (count > 0) {
          if (!disposed) setIndexReady(true);
          return;
        }
      } catch (e) {
        console.error(e);
      }
      if (!disposed && tries < INDEX_POLL_MAX_TRIES) {
        tries += 1;
        window.setTimeout(poll, INDEX_POLL_MS);
      }
    };

    poll();
    return () => {
      disposed = true;
    };
  }, []);

  // -------------------------------------------------------------- search ---
  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    const seq = ++seqRef.current;
    const trimmed = query.trim();

    if (!trimmed) {
      // Empty query: a clean idle bar. (Clipboard history used to live here;
      // the feature was removed, so there is nothing to show.)
      setLoading(false);
      setResults([]);
      setSelected(0);
      return;
    }

    setLoading(true);
    debounceRef.current = window.setTimeout(() => {
      invoke<SearchResult[]>("search_items", { query: trimmed })
        .then((res) => {
          if (seq !== seqRef.current) return; // stale response
          setResults(res);
          setSelected(0);
        })
        .catch(console.error)
        .finally(() => {
          if (seq === seqRef.current) setLoading(false);
        });
    }, 50);

    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [query, indexReady]);

  // ---------------------------------------------------------- derived data --
  const activeEngine = SEARCH_ENGINES[engine] ?? SEARCH_ENGINES.google;
  const combined = useMemo<SearchResult[]>(() => {
    const list = [...results];
    const trimmed = query.trim();
    if (trimmed) {
      list.push({
        name: `Search ${activeEngine.label} for "${trimmed}"`,
        path: `${activeEngine.url}${encodeURIComponent(trimmed)}`,
        kind: "web",
        score: 0,
      });
    }
    return list;
  }, [results, query, activeEngine]);

  const { groups, flat } = useMemo(() => groupResults(combined), [combined]);

  const isExpanded = query.trim().length > 0 || results.length > 0;
  const hasResults = flat.length > 0;
  const trimmedQuery = query.trim();
  const isKillQuery = trimmedQuery.toLowerCase().startsWith("kill ");
  const hasKillResult = flat.some((item) => item.kind === "kill");

  // --------------------------------------------------------------- toast ----
  const showToast = useCallback((message: string) => {
    setToast(message);
    if (toastTimer.current) clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 4000);
  }, []);

  useEffect(
    () => () => {
      if (toastTimer.current) clearTimeout(toastTimer.current);
      if (copiedTimer.current) clearTimeout(copiedTimer.current);
      if (navTimer.current) clearTimeout(navTimer.current);
    },
    [],
  );

  // ------------------------------------------------------------- actions ----
  const openSettings = async () => {
    try {
      await invoke("open_settings_window");
      await invoke("hide_window"); // Hide search bar while settings is open
    } catch (e) {
      console.error(e);
    }
  };

  const openContainingFolder = useCallback(
    async (item?: SearchResult) => {
      if (!item) return;
      const raw = item.path;
      const split = Math.max(raw.lastIndexOf("\\"), raw.lastIndexOf("/"));
      if (split <= 0) return;
      try {
        await invoke("open_path", { path: raw.slice(0, split) });
      } catch (e) {
        showToast(String(e));
      }
    },
    [showToast],
  );

  const launchItem = useCallback(
    async (idx: number) => {
      const item = flat[idx];
      if (!item) return;

      try {
        if (item.kind === "math") {
          await invoke("copy_to_clipboard", { text: item.path });
          setCopiedIdx(idx);
          if (copiedTimer.current) clearTimeout(copiedTimer.current);
          copiedTimer.current = window.setTimeout(() => {
            setCopiedIdx(null);
            invoke("hide_window").catch(console.error);
          }, 800);
        } else if (item.kind === "kill") {
          await invoke("kill_process", { name: item.path });
          await invoke("hide_window");
        } else {
          await invoke("launch_item", { path: item.path });
          await invoke("hide_window");
        }
      } catch (e) {
        showToast(String(e));
      }
    },
    [flat, focusInput, showToast],
  );

  // ----------------------------------------------------------- keyboard -----
  const handleKey = useCallback(
    (e: KeyboardEvent) => {
      const input = inputRef.current;
      const inSearchBox = !!input && e.target === input;

      // Type-through safety net: if the window is active but the search box is
      // not focused, route ordinary keystrokes into it anyway so typing always
      // works, without requiring a click first.
      if (!inSearchBox && !e.ctrlKey && !e.metaKey && !e.altKey && !e.repeat && input) {
        const printable = e.key.length === 1;
        const isBackspace = e.key === "Backspace";
        if (printable || isBackspace) {
          e.preventDefault();
          input.focus({ preventScroll: true });

          const value = input.value;
          const caret = input.selectionStart ?? value.length;
          const selEnd = input.selectionEnd ?? caret;
          let next: string;
          let pos: number;

          if (isBackspace) {
            if (caret === 0 && selEnd === 0) return;
            if (selEnd > caret) {
              next = value.slice(0, caret) + value.slice(selEnd);
              pos = caret;
            } else {
              const prefixChars = Array.from(value.slice(0, caret));
              prefixChars.pop();
              const prefix = prefixChars.join("");
              next = prefix + value.slice(selEnd);
              pos = prefix.length;
            }
          } else {
            next = value.slice(0, caret) + e.key + value.slice(selEnd);
            pos = caret + e.key.length;
          }

          setQuery(next);
          requestAnimationFrame(() => {
            const el = inputRef.current;
            if (el) el.setSelectionRange(pos, pos);
          });
          return;
        }
      }

      if (e.key === "Escape") {
        e.preventDefault();
        if (query === "") {
          invoke("hide_window").catch(console.error);
        } else {
          setQuery("");
        }
        return;
      }

      const total = flat.length;

      if (e.key === "Enter") {
        if (e.repeat) return;
        e.preventDefault();
        if (total === 0) return;
        if (e.shiftKey) {
          openContainingFolder(flat[selected]);
        } else {
          launchItem(selected);
        }
        return;
      }

      const navKeys = ["ArrowDown", "ArrowUp", "Home", "End", "PageDown", "PageUp"];
      if (navKeys.includes(e.key)) {
        if (total === 0) return;
        e.preventDefault();

        // Suppress mouse-hover selection for a moment after keyboard navigation
        isKeyboardNav.current = true;
        if (navTimer.current) clearTimeout(navTimer.current);
        navTimer.current = window.setTimeout(() => {
          isKeyboardNav.current = false;
        }, 200);

        switch (e.key) {
          case "ArrowDown":
            setSelected((s) => (s + 1) % total);
            break;
          case "ArrowUp":
            setSelected((s) => (s - 1 + total) % total);
            break;
          case "Home":
            setSelected(0);
            break;
          case "End":
            setSelected(total - 1);
            break;
          case "PageDown":
            setSelected((s) => Math.min(total - 1, s + 8));
            break;
          case "PageUp":
            setSelected((s) => Math.max(0, s - 8));
            break;
        }
      }
    },
    [query, flat, selected, launchItem, openContainingFolder],
  );

  useEffect(() => {
    window.addEventListener("keydown", handleKey);
    return () => window.removeEventListener("keydown", handleKey);
  }, [handleKey]);

  useEffect(() => {
    // Smooth glide for keyboard navigation, instant jump for mouse hover:
    // smooth-scrolling on every hover made the list drift while browsing.
    const selectedEl = document.querySelector(".result-item.selected");
    selectedEl?.scrollIntoView({
      block: "nearest",
      behavior: isKeyboardNav.current ? "smooth" : "auto",
    });
  }, [selected]);

  // --------------------------------------------------------------- render ---
  let emptyState: React.ReactNode = null;
  if (!hasResults && isExpanded) {
    if (loading) {
      emptyState = <div className="empty-state">Searching&hellip;</div>;
    } else if (!indexReady) {
      emptyState = <div className="empty-state">Building the index&hellip;</div>;
    } else if (isKillQuery && !hasKillResult) {
      emptyState = <div className="empty-state">No matching window found</div>;
    } else {
      emptyState = <div className="empty-state">No results - Enter searches the web</div>;
    }
  }

  return (
    <div
      className={`container ${isExpanded ? "expanded" : ""}`}
    >
      <div className="search-header">
        <div className={`search-icon ${loading ? "spinning" : ""}`} aria-hidden="true">
          <Search24Regular />
        </div>
        <input
          ref={inputRef}
          className="search-input"
          placeholder="Search apps, settings, files and web..."
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            // Handled by the window-level listener; stop the input from doing
            // anything extra with Enter/arrows (e.g. caret moves, submits).
            if (["Enter", "ArrowUp", "ArrowDown", "Escape"].includes(e.key)) e.preventDefault();
          }}
          spellCheck={false}
          autoComplete="off"
          aria-label="Search"
          role="combobox"
          aria-expanded={isExpanded}
          aria-controls="result-list"
          aria-autocomplete="list"
          aria-activedescendant={hasResults ? `result-${selected}` : undefined}
        />
        <button
          className="theme-toggle"
          onClick={openSettings}
          onMouseDown={(e) => e.preventDefault()} // keep focus in the search box
          title="Open Settings"
          aria-label="Open Settings"
          type="button"
        >
          <Settings20Regular />
        </button>
      </div>

      <div className="results-region" id="result-list" role="listbox" aria-label="Search results">
        <div className="results-inner">
          <div className="results-list">
            {emptyState}
            {groups.map((group) => (
              <div className="result-group" role="group" aria-label={group.kind} key={group.kind}>
                <div className="result-group-title" aria-hidden="true">
                  {group.kind.toUpperCase()}
                </div>
                {group.items.map(({ item, idx }) => (
                  <div
                    key={`${item.kind}-${item.path}`}
                    id={`result-${idx}`}
                    role="option"
                    aria-selected={selected === idx}
                    className={`result-item ${selected === idx ? "selected" : ""}`}
                    onMouseDown={(e) => e.preventDefault()} // keep focus in the search box
                    onClick={() => launchItem(idx)}
                    onMouseMove={() => {
                      if (!isKeyboardNav.current && selected !== idx) setSelected(idx);
                    }}
                  >
                    <div className="result-icon" aria-hidden="true">
                      {item.icon_base64 ? (
                        <img src={`data:image/png;base64,${item.icon_base64}`} alt="" />
                      ) : (
                        KIND_ICONS[item.kind]
                      )}
                    </div>
                    <div className="result-info">
                      <div className="result-name">{item.name}</div>
                    </div>
                    {copiedIdx === idx && <span className="copied-label">Copied!</span>}
                    {item.kind === "app" && (
                      <button
                        type="button"
                        className="action-btn"
                        title="Open containing folder (Shift+Enter)"
                        aria-label="Open containing folder"
                        onMouseDown={(e) => e.preventDefault()}
                        onClick={(e) => {
                          e.stopPropagation();
                          openContainingFolder(item);
                        }}
                      >
                        <FolderOpen20Regular />
                      </button>
                    )}
                  </div>
                ))}
              </div>
            ))}
          </div>
        </div>
      </div>

      {toast && (
        <div className="toast" role="status">
          {toast}
        </div>
      )}

      <div className="footer">
        <div className="footer-keys">
          <div className="footer-item">
            <span className="key-hint">&uarr;&darr;</span>
            <span className="hint-label">Move</span>
          </div>
          <div className="footer-item">
            <span className="key-hint">Enter</span>
            <span className="hint-label">Open</span>
          </div>
          <div className="footer-item">
            <span className="key-hint">Shift&thinsp;Enter</span>
            <span className="hint-label">Folder</span>
          </div>
          <div className="footer-item">
            <span className="key-hint">Esc</span>
            <span className="hint-label">Clear</span>
          </div>
        </div>
        <div className="footer-brand">GLIMPSE &bull; {hotkey}</div>
      </div>
    </div>
  );
}
