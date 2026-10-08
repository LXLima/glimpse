import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { applyThemeVariables } from "./utils/theme";
import { Dismiss20Regular } from "@fluentui/react-icons";
import "./App.css";
import "./Settings.css";

interface FullConfig {
  hotkey: string;
  theme: string;
  startup: boolean;
  search_engine: string;
}

const SEARCH_ENGINE_OPTIONS = [
  { id: "google", label: "Google" },
  { id: "bing", label: "Bing" },
  { id: "duckduckgo", label: "DuckDuckGo" },
  { id: "brave", label: "Brave" },
];

const THEMES = [
  "amoled", "aura", "ayu", "carbonfox", "catppuccin-frappe", "catppuccin-macchiato",
  "catppuccin", "cobalt2", "cursor", "dracula", "everforest", "flexoki", "github",
  "gruvbox", "kanagawa", "lucent-orng", "material", "matrix", "mercury", "monokai",
  "nightowl", "nord", "oc-2", "one-dark", "onedarkpro", "orng", "osaka-jade",
  "palenight", "rosepine", "shadesofpurple", "solarized", "synthwave84",
  "tokyonight", "vercel", "vesper", "zenburn"
];

const DEFAULT_HOTKEY = "Ctrl+Space";

/** Codes that only ever mean "a modifier is held" - never a complete hotkey. */
const MODIFIER_CODES = new Set([
  "ControlLeft", "ControlRight",
  "ShiftLeft", "ShiftRight",
  "AltLeft", "AltRight",
  "MetaLeft", "MetaRight",
]);

/** Turns a keyboard code token (as produced by `KeyboardEvent.code`) into
 * something human readable. The stored value stays in code form because that
 * is exactly what the backend's shortcut parser expects. */
function prettyKey(token: string): string {
  const letter = /^KEY([A-Z])$/.exec(token);
  if (letter) return letter[1];
  const digit = /^DIGIT(\d)$/.exec(token);
  if (digit) return digit[1];

  switch (token) {
    case "SUPER": return "Win";
    case "CONTROL":
    case "CTRL": return "Ctrl";
    case "ALT": return "Alt";
    case "SHIFT": return "Shift";
    case "SPACE": return "Space";
    case "ESCAPE": return "Esc";
    case "ENTER": return "Enter";
    case "TAB": return "Tab";
    case "BACKSPACE": return "Backspace";
    case "DELETE": return "Del";
    case "INSERT": return "Ins";
    case "ARROWUP": return "\u2191";
    case "ARROWDOWN": return "\u2193";
    case "ARROWLEFT": return "\u2190";
    case "ARROWRIGHT": return "\u2192";
    case "PAGEUP": return "PgUp";
    case "PAGEDOWN": return "PgDn";
    case "EQUAL": return "=";
    case "MINUS": return "-";
    case "COMMA": return ",";
    case "PERIOD": return ".";
    case "SLASH": return "/";
    case "BACKSLASH": return "\\";
    case "SEMICOLON": return ";";
    case "QUOTE": return "'";
    case "BACKQUOTE": return "`";
    case "BRACKETLEFT": return "[";
    case "BRACKETRIGHT": return "]";
    default: return token;
  }
}

function prettyHotkey(hotkey: string): string {
  return hotkey.split("+").map(prettyKey).join(" + ");
}

export default function Settings() {
  const [config, setConfig] = useState<FullConfig | null>(null);
  const [tempTheme, setTempTheme] = useState<string>("system");
  const [tempEngine, setTempEngine] = useState<string>("google");
  const [isRecording, setIsRecording] = useState(false);
  const [recordingHint, setRecordingHint] = useState<string | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  useEffect(() => {
    invoke<FullConfig>("get_full_config").then((cfg) => {
      setConfig(cfg);
      setTempTheme(cfg.theme);
      setTempEngine(cfg.search_engine);
    }).catch(console.error);
  }, []);

  useEffect(() => {
    applyThemeVariables(tempTheme);
  }, [tempTheme]);

  const handleSave = async () => {
    if (!config || isRecording) return;
    try {
      await invoke("save_full_config", { config: { ...config, theme: tempTheme, search_engine: tempEngine } });
      await invoke("close_settings_window");
    } catch (e) {
      setErrorMsg(String(e));
    }
  };

  const handleCancel = async () => {
    await invoke("close_settings_window");
  };

  const handleDrag = async () => {
    await invoke("start_settings_window_drag");
  };

  // ---------------------------------------------------------- recording ----
  const startRecording = useCallback(() => {
    // Clear focus from the Record/Reset buttons: a focused button turns
    // Space/Enter into a second click mid-recording. Keystrokes are captured
    // at window level, so focus is not needed.
    if (document.activeElement instanceof HTMLElement) {
      document.activeElement.blur();
    }
    setIsRecording(true);
    setErrorMsg(null);
    setRecordingHint(null);
  }, []);

  const cancelRecording = useCallback(() => {
    setIsRecording(false);
    setRecordingHint(null);
  }, []);

  // Release the global hotkey while recording: RegisterHotKey swallows the
  // bound combination before it ever reaches this window, which used to leave
  // the recorder stuck on "Recording..." forever.
  useEffect(() => {
    if (!isRecording) return;
    invoke("set_hotkey_paused", { paused: true }).catch(console.error);
    return () => {
      invoke("set_hotkey_paused", { paused: false }).catch(console.error);
    };
  }, [isRecording]);

  // Clicking anywhere outside the hotkey field aborts the recording instead of
  // silently capturing a shortcut the user did not intend to change.
  useEffect(() => {
    if (!isRecording) return;
    const onPointerDown = (e: MouseEvent) => {
      const wrapper = document.querySelector(".hotkey-input-wrapper");
      if (wrapper && !wrapper.contains(e.target as Node)) {
        setIsRecording(false);
      }
    };
    window.addEventListener("mousedown", onPointerDown);
    return () => window.removeEventListener("mousedown", onPointerDown);
  }, [isRecording]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!isRecording) return;
      e.preventDefault();
      e.stopPropagation();
      if (e.repeat) return;

      // Escape aborts the recording - it is never a valid hotkey on its own.
      if (e.code === "Escape") {
        cancelRecording();
        return;
      }

      // Wait for a non-modifier key - but show what is being held so far.
      if (MODIFIER_CODES.has(e.code)) {
        const held: string[] = [];
        if (e.ctrlKey) held.push("CTRL");
        if (e.altKey) held.push("ALT");
        if (e.shiftKey) held.push("SHIFT");
        if (e.metaKey) held.push("SUPER");
        setRecordingHint(held.length ? `${prettyHotkey(held.join("+"))} \u2026` : null);
        return;
      }

      const modifiers: string[] = [];
      if (e.ctrlKey) modifiers.push("CTRL");
      if (e.altKey) modifiers.push("ALT");
      if (e.shiftKey) modifiers.push("SHIFT");
      if (e.metaKey) modifiers.push("SUPER");

      if (modifiers.length === 0) {
        setErrorMsg("A hotkey needs Ctrl, Alt, or the Windows (Super) key.");
        return;
      }

      if (!e.code) {
        setErrorMsg("That key combination is not supported.");
        return;
      }

      // `e.code` (not `e.key`) is used so Shift/Ctrl combinations and non-US
      // layouts map onto keys the backend parser actually understands.
      const combo = [...modifiers, e.code.toUpperCase()].join("+");

      invoke("validate_hotkey", { hotkey: combo })
        .then(() => {
          setConfig((prev) => (prev ? { ...prev, hotkey: combo } : prev));
          setIsRecording(false);
          setErrorMsg(null);
          setRecordingHint(null);
          // Leave the button focused, otherwise the next Space re-opens the
          // recorder.
          if (document.activeElement instanceof HTMLElement) {
            document.activeElement.blur();
          }
        })
        .catch((err) => {
          setErrorMsg(String(err));
        });
    },
    [isRecording, cancelRecording],
  );

  useEffect(() => {
    if (!isRecording) return;
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [isRecording, handleKeyDown]);

  // Escape closes the settings window (unless a recording is in progress).
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (isRecording) return;
      if (e.target instanceof HTMLButtonElement || e.target instanceof HTMLSelectElement) return;
      if (e.key === "Escape") {
        e.preventDefault();
        handleCancel();
      }
      if (e.key === "Enter" && !e.repeat) {
        e.preventDefault();
        handleSave();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  if (!config) {
    return (
      <div className="settings-container">
        <div className="settings-card loading">Loading&hellip;</div>
      </div>
    );
  }

  return (
    <div className="settings-container">
      <div className="settings-card">
      <div className="settings-header" onMouseDown={handleDrag}>
        <h2>Settings</h2>
        <button
          type="button"
          className="settings-close"
          title="Close"
          aria-label="Close settings"
          onMouseDown={(e) => e.stopPropagation()} // never start a drag from here
          onClick={handleCancel}
        >
          <Dismiss20Regular />
        </button>
      </div>

      <div className="settings-content">
        {errorMsg && (
          <div className="settings-error" role="alert" onClick={() => setErrorMsg(null)}>
            {errorMsg}
          </div>
        )}

        <div className="settings-group">
          <label htmlFor="theme-select">Theme</label>
          <select
            id="theme-select"
            className="theme-select"
            value={tempTheme}
            onChange={(e) => setTempTheme(e.target.value)}
          >
            <option value="system">System Default</option>
            <option value="light">Default Light</option>
            <option value="dark">Default Dark</option>
            {THEMES.map((t) => (
              <option key={t} value={t}>{t}</option>
            ))}
          </select>
        </div>

        <div className="settings-group">
          <label htmlFor="engine-select">Search Engine</label>
          <select
            id="engine-select"
            className="theme-select"
            value={tempEngine}
            onChange={(e) => setTempEngine(e.target.value)}
          >
            {SEARCH_ENGINE_OPTIONS.map((e) => (
              <option key={e.id} value={e.id}>{e.label}</option>
            ))}
          </select>
          <small className="settings-hint">
            Used for the fallback result at the bottom of the list.
          </small>
        </div>

        <div className="settings-group">
          <label htmlFor="hotkey-input">Global Hotkey</label>
          <div className="hotkey-input-wrapper">
            <input
              id="hotkey-input"
              type="text"
              readOnly
              value={
                isRecording
                  ? recordingHint ?? "Press shortcut\u2026"
                  : prettyHotkey(config.hotkey)
              }
              className={`hotkey-input ${isRecording ? "recording" : ""}`}
              onClick={() => {
                if (!isRecording) startRecording();
              }}
              aria-label="Global hotkey"
            />
            <button
              type="button"
              className={`hotkey-btn ${isRecording ? "active" : ""}`}
              onMouseDown={(e) => e.preventDefault()} // never steal focus; Space must reach the recorder
              onClick={() => (isRecording ? cancelRecording() : startRecording())}
            >
              {isRecording ? "Cancel" : "Record"}
            </button>
            <button
              type="button"
              className="hotkey-btn"
              title="Restore the default hotkey"
              onMouseDown={(e) => e.preventDefault()} // same as above
              onClick={() => {
                setConfig((prev) => (prev ? { ...prev, hotkey: DEFAULT_HOTKEY } : prev));
                setErrorMsg(null);
                setIsRecording(false);
              }}
            >
              Reset
            </button>
          </div>
          <small className="settings-hint">
            {isRecording
              ? "Press a combination that includes Ctrl, Alt, or Windows \u2014 Esc cancels."
              : "Requires Ctrl, Alt, or Windows (Super) key."}
          </small>
        </div>

        <div className="settings-group checkbox-group">
          <label className="checkbox-label">
            <input
              type="checkbox"
              checked={config.startup}
              onChange={(e) => setConfig({ ...config, startup: e.target.checked })}
            />
            Start Glimpse automatically on login
          </label>
        </div>
      </div>

      <div className="settings-footer">
        <button className="btn-cancel" onClick={handleCancel}>Cancel</button>
        <button
          className="btn-save"
          onClick={handleSave}
          disabled={isRecording}
          title={isRecording ? "Finish recording the hotkey first" : "Save settings"}
        >
          Save
        </button>
      </div>
      </div>
    </div>
  );
}
