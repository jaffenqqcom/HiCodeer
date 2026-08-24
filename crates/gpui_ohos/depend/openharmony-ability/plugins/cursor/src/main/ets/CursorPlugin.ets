import {
  AsyncPluginBase,
  BridgeCallContext,
  BridgeContextRequirement,
  BridgePluginHookContext,
  BridgeTypedValue,
} from "@ohos-rs/ability";

const WINDOW_ID_EVENT = "window-id";
const WINDOW_ID_RESPONSE_TYPE = "ohos.cursor.WindowIdResponse";

/**
 * Mouse cursor plugin.
 *
 * Pushes the main-window id to Rust once from `onInstall` (window-stage ready):
 * the inbound event sink is attached before window-stage activation, so the id
 * reaches Rust before any cursor-style change. Rust performs every subsequent
 * change through OH_Input_SetPointerStyle — no ArkTS call is involved.
 */
export class CursorPlugin extends AsyncPluginBase {
  readonly id = "ohos.cursor";
  readonly requires: BridgeContextRequirement[] = ["window-stage"];

  override onInstall(context: BridgePluginHookContext): void {
    let windowId = -1;
    try {
      // Prefer the window that owns the attached DefaultXComponent.
      windowId = context.getWindow().getWindowProperties().id;
    } catch {
      // Fall back to the Ability-level main window when the component is not attached yet.
      windowId = context.getWindowStage().getMainWindowSync().getWindowProperties().id;
    }
    context.invokeNativeSync(
      WINDOW_ID_EVENT,
      "std.i32",
      WINDOW_ID_RESPONSE_TYPE,
      windowId,
    );
  }

  async invokeAsync(
    action: string,
    _request: BridgeTypedValue,
    _context: BridgeCallContext,
  ): Promise<BridgeTypedValue> {
    throw new Error(`Unsupported ohos.cursor action '${action}'`);
  }
}
