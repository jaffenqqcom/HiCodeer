import inputMethod from "@ohos.inputMethod";
import display from "@ohos.display";
import {
  AsyncPluginBase,
  BridgeCallContext,
  BridgeContextRequirement,
  BridgePluginHookContext,
  BridgeTypedValue,
} from "@ohos-rs/ability";

const IME_ID = "ohos.ime";
const INSERT_TEXT = "insert-text";
const DELETE_LEFT = "delete-left";
const DELETE_RIGHT = "delete-right";
const FUNCTION_KEY = "function-key";
const KEYBOARD_STATUS = "keyboard-status";
const PREVIEW_TEXT = "preview-text";
const ACK = "ohos.ime.Ack";
const CURSOR_REQUEST = "ohos.ime.CursorRequest";

const ATTACH_ACTION = "attach";
const DETACH_ACTION = "detach";
const UPDATE_CURSOR_ACTION = "update-cursor";

// `attachWithUIContext` is rejected while the edit box has not taken focus yet
// (error 12800003). That focus hand-off routinely outlives a single frame at
// start-up, so the retry budget has to span roughly a second rather than a few
// frames. Retries are unconditional because the error code is not reliably
// surfaced on every OpenHarmony build.
const ATTACH_RETRY_LIMIT = 10;
const ATTACH_RETRY_DELAY_MS = 120;

// Rust reports the cursor in *window* coordinates: it already folds in the
// XComponent's offset inside the window (status-bar / safe-area insets - whatever
// the system reports via `xc.offset`). The one layer it cannot see is the window
// title bar, which sits ABOVE the XComponent's content area, so we add it here.
// `winTop` (windowRect.top) covers the status bar, `titleBarHeightPx()` covers the
// title bar. The trims below are manual correction for any residual error. Units
// are physical px.
const TITLE_BAR_HEIGHT_VP = 46;
const CURSOR_TRIM_X_PX = 0;
const CURSOR_TRIM_Y_PX = 0;

const TEXT_CONFIG: inputMethod.TextConfig = {
  inputAttribute: {
    textInputType: inputMethod.TextInputType.TEXT,
    enterKeyType: inputMethod.EnterKeyType.DONE,
  },
};

interface ImeInsertText {
  text: string;
}
interface ImeDeleteLeft {
  length: number;
}
interface ImeDeleteRight {
  length: number;
}
interface ImeFunctionKey {
  key: number;
}
interface ImeKeyboardStatus {
  status: number;
}
interface ImePreviewText {
  text: string;
  start: number;
  end: number;
}
interface ImeCursorRequest {
  x: number;
  y: number;
  width: number;
  height: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise<void>((resolve: (value: void) => void): void => {
    setTimeout((): void => resolve(), ms);
  });
}

/**
 * IME (soft keyboard) capability plugin.
 *
 * ArkTS owns the OpenHarmony `InputMethodController` (`@kit.IMEKit`). IME input
 * callbacks are forwarded to Rust through `invokeNativeSync` (-> ohos.ime
 * main-thread events), and Rust drives session lifecycle through `invokeAsync`
 * (attach / detach / update-cursor). The candidate box follows the editor cursor:
 * Rust pushes the cursor rect on every caret move, and this plugin recomputes its
 * screen position on window geometry changes.
 *
 * Two invariants matter for correctness:
 *
 * - `InputMethodController` is a process singleton and `controller.on(..)` is
 *   additive, so callbacks are registered exactly once for the plugin's lifetime.
 *   Registering per-attach makes every keystroke fan out into N duplicate
 *   `insert-text` events (observed as one character typed -> several inserted).
 * - `attach()` is idempotent and reports its real outcome, so Rust can retry the
 *   binding until the edit box has actually taken focus.
 */
export class ImePlugin extends AsyncPluginBase {
  readonly id = IME_ID;
  readonly requires: BridgeContextRequirement[] = ["ui-context"];

  private hookContext?: BridgePluginHookContext;
  private controller?: inputMethod.InputMethodController;
  private lastCursor?: ImeCursorRequest;
  private attached = false;
  private callbacksRegistered = false;
  private attachInFlight?: Promise<boolean>;

  override onInstall(context: BridgePluginHookContext): void {
    this.hookContext = context;
    // Refresh the candidate box when the window moves or resizes; Rust does not
    // re-push the cursor on those transitions, so we recompute from the cache.
    try {
      const win = context.getWindow();
      const refresh = (): void => this.refreshCursorWithLatest();
      win.on("windowSizeChange", refresh);
      win.on("windowRectChange", refresh);
    } catch (error) {
      console.error(`ohos.ime: failed to register window listeners: ${String(error)}`);
    }
  }

  override onDispose(_context: BridgePluginHookContext): void {
    this.stopInputSession();
    this.hookContext = undefined;
    this.controller = undefined;
    this.lastCursor = undefined;
  }

  async invokeAsync(
    action: string,
    payload: BridgeTypedValue,
    _context: BridgeCallContext,
  ): Promise<BridgeTypedValue> {
    if (action === ATTACH_ACTION) {
      const accepted = await this.attach();
      return { typeName: ACK, value: { accepted } };
    }
    if (action === DETACH_ACTION) {
      this.stopInputSession();
      return { typeName: ACK, value: { accepted: true } };
    }
    if (action === UPDATE_CURSOR_ACTION) {
      this.lastCursor = this.parseCursor(payload);
      this.updateCursor();
      return { typeName: ACK, value: { accepted: true } };
    }
    throw new Error(`Unsupported ohos.ime action '${action}'`);
  }

  private parseCursor(payload: BridgeTypedValue): ImeCursorRequest {
    if (payload.typeName !== CURSOR_REQUEST || typeof payload.value !== "object") {
      throw new Error(`ohos.ime.update-cursor requires bridge type ${CURSOR_REQUEST}`);
    }
    return payload.value as ImeCursorRequest;
  }

  /**
   * Binds the system IME, retrying while the edit box has not taken focus yet.
   *
   * Concurrent callers share a single attempt. A request that arrives while the
   * binding is already marked attached still has to re-bind: hiding the window
   * (minimize) makes the system drop the session while `this.attached` stays
   * true, so short-circuiting here would report success without restoring the
   * keyboard. Registration of the IME callbacks stays guarded separately by
   * `callbacksRegistered`, so re-binding never stacks a second handler set.
   */
  private attach(): Promise<boolean> {
    if (this.attachInFlight) {
      return this.attachInFlight;
    }
    this.attachInFlight = this.bindWithRetries(ATTACH_RETRY_LIMIT).then((ok: boolean): boolean => {
      this.attachInFlight = undefined;
      return ok;
    });
    return this.attachInFlight;
  }

  private async bindWithRetries(retry: number): Promise<boolean> {
    const context = this.hookContext;
    if (!context) {
      console.error("ohos.ime: attach called before onInstall");
      return false;
    }
    const controller = this.ensureController();
    if (!controller) {
      return false;
    }
    try {
      controller.attachWithUIContext(context.getUIContext(), TEXT_CONFIG);
      // Registration is guarded by `callbacksRegistered`, so retrying never
      // stacks a second handler set on the same controller.
      this.registerCallbacksOnce();
      this.attached = true;
    } catch (error) {
      if (retry > 0) {
        await sleep(ATTACH_RETRY_DELAY_MS);
        return this.bindWithRetries(retry - 1);
      }
      console.error(`ohos.ime: attach failed after ${ATTACH_RETRY_LIMIT} attempts: ${String(error)}`);
      return false;
    }

    // Bound. Everything below is best-effort: a failure here must never fall
    // back to re-attaching, because that would register a second callback set.
    try {
      controller.showTextInput();
    } catch (error) {
      console.error(`ohos.ime: showTextInput failed: ${String(error)}`);
    }
    this.updateCursor();
    return true;
  }

  private ensureController(): inputMethod.InputMethodController | undefined {
    if (!this.controller) {
      try {
        this.controller = inputMethod.getController();
      } catch (error) {
        console.error(`ohos.ime: getController failed: ${String(error)}`);
        return undefined;
      }
    }
    return this.controller;
  }

  /**
   * Registers the IME -> Rust callbacks exactly once.
   *
   * `controller.on(..)` accumulates handlers and offers no way to unregister, so
   * calling this per-attach multiplies every keystroke by the attach count.
   */
  private registerCallbacksOnce(): void {
    if (this.callbacksRegistered) {
      return;
    }
    const controller = this.controller;
    if (!controller) {
      return;
    }
    controller.on("insertText", (text: string): void => {
      this.hookContext?.invokeNativeSync(INSERT_TEXT, "ohos.ime.InsertText", ACK, { text });
    });
    controller.on("deleteLeft", (length: number): void => {
      this.hookContext?.invokeNativeSync(DELETE_LEFT, "ohos.ime.DeleteLeft", ACK, { length });
    });
    controller.on("deleteRight", (length: number): void => {
      this.hookContext?.invokeNativeSync(DELETE_RIGHT, "ohos.ime.DeleteRight", ACK, { length });
    });
    controller.on("sendFunctionKey", (functionKey: inputMethod.FunctionKey): void => {
      this.hookContext?.invokeNativeSync(FUNCTION_KEY, "ohos.ime.FunctionKey", ACK, {
        key: functionKey.enterKeyType,
      });
    });
    controller.on("setPreviewText", (text: string, range: inputMethod.Range): void => {
      this.hookContext?.invokeNativeSync(PREVIEW_TEXT, "ohos.ime.PreviewText", ACK, {
        text: text ?? "",
        start: range.start ?? 0,
        end: range.end ?? 0,
      });
    });
    controller.on("sendKeyboardStatus", (status: inputMethod.KeyboardStatus): void => {
      this.hookContext?.invokeNativeSync(KEYBOARD_STATUS, "ohos.ime.KeyboardStatus", ACK, {
        status: status as number,
      });
    });
    this.callbacksRegistered = true;
  }

  private stopInputSession(): void {
    // Keep `controller` and its callbacks: they cannot be unregistered, and
    // reusing them across sessions is what prevents duplicate callbacks.
    this.attached = false;
    try {
      this.controller?.stopInputSession();
    } catch (error) {
      console.error(`ohos.ime: stopInputSession failed: ${String(error)}`);
    }
  }

  private updateCursor(): void {
    if (!this.attached || !this.controller || !this.lastCursor) {
      return;
    }
    try {
      this.controller.updateCursor(this.computeCursorScreenPos());
    } catch (error) {
      console.error(`ohos.ime: updateCursor failed: ${String(error)}`);
    }
  }

  private refreshCursorWithLatest(): void {
    this.updateCursor();
  }

  /**
   * Height of the window title bar in physical px.
   *
   * The title bar is drawn above the XComponent's content area, so the cursor
   * offset Rust reports (relative to the XComponent surface) never includes it.
   * OpenHarmony has no direct title-bar-height API, so we express it in vp and
   * convert with the display density - `TITLE_BAR_HEIGHT_VP` is the single knob
   * to tune if the candidate box is still misaligned vertically.
   */
  private titleBarHeightPx(): number {
    try {
      return TITLE_BAR_HEIGHT_VP * display.getDefaultDisplaySync().densityPixels;
    } catch {
      return 0;
    }
  }

  private computeCursorScreenPos(): inputMethod.CursorInfo {
    if (!this.lastCursor) {
      return { left: 0, top: 0, width: 0, height: 0 };
    }
    let winLeft = 0;
    let winTop = 0;
    try {
      const rect = this.hookContext?.getWindow().getWindowProperties().windowRect;
      winLeft = rect?.left ?? 0;
      winTop = rect?.top ?? 0;
    } catch {
      // Window not ready; fall back to origin.
    }
    // Rust hands us window-relative physical px (it already added the
    // XComponent's offset inside the window, i.e. the status bar / insets). The
    // title bar sits above the XComponent's content area and is invisible to that
    // offset, so we add it here on top of the window's screen position.
    const titleBar = this.titleBarHeightPx();
    const left = this.lastCursor.x + winLeft + CURSOR_TRIM_X_PX;
    const top = this.lastCursor.y + winTop + titleBar + CURSOR_TRIM_Y_PX;
    return {
      left,
      top,
      width: this.lastCursor.width,
      height: this.lastCursor.height,
    };
  }
}
