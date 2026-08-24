import { BuilderNode } from "@ohos.arkui.node";
import {
  AsyncPluginBase,
  BridgeCallContext,
  BridgeContextRequirement,
  BridgePluginHookContext,
  BridgeTypedValue,
} from "@ohos-rs/ability";

const PINCH_SAMPLE_TYPE = "ohos.pinch.PinchSample";
const PINCH_ACK_TYPE = "ohos.pinch.PinchAck";

const PINCH_BEGIN_EVENT = "pinch-begin";
const PINCH_UPDATE_EVENT = "pinch-update";
const PINCH_END_EVENT = "pinch-end";

interface PinchSample {
  scale: number;
  centerX: number;
  centerY: number;
}

/**
 * Forwards a pinch sample to Rust through the synchronous main-thread bridge.
 * The overlay stays hit-test transparent so clicks/scrolling pass through to the
 * underlying XComponent; only the two-finger pinch gesture is captured here.
 */
class PinchHandler {
  private readonly context: BridgePluginHookContext;

  constructor(context: BridgePluginHookContext) {
    this.context = context;
  }

  onEvent(action: string, scale: number, centerX: number, centerY: number): void {
    const sample: PinchSample = { scale, centerX, centerY };
    try {
      this.context.invokeNativeSync(action, PINCH_SAMPLE_TYPE, PINCH_ACK_TYPE, sample as ESObject);
    } catch (error) {
      console.error(`ohos.pinch ${action} invokeNativeSync failed: ${String(error)}`);
    }
  }
}

@Builder
function PinchOverlay(handler: PinchHandler) {
  Stack()
    .width("100%")
    .height("100%")
    .hitTestBehavior(HitTestMode.Transparent)
    .gesture(
      PinchGesture({ fingers: 2 })
        .onActionStart((event: GestureEvent) => {
          handler.onEvent(
            PINCH_BEGIN_EVENT,
            event.scale,
            event.pinchCenterX,
            event.pinchCenterY,
          );
        })
        .onActionUpdate((event: GestureEvent) => {
          handler.onEvent(
            PINCH_UPDATE_EVENT,
            event.scale,
            event.pinchCenterX,
            event.pinchCenterY,
          );
        })
        .onActionEnd((event: GestureEvent) => {
          handler.onEvent(PINCH_END_EVENT, 1, 0, 0);
        })
    )
}

const pinchBuilder = wrapBuilder<[PinchHandler]>(PinchOverlay);

/**
 * Pinch gesture capability plugin.
 *
 * Attaches a transparent full-screen overlay with a two-finger PinchGesture and
 * pushes every gesture sample to Rust (ohos.pinch) via invokeNativeSync. Rust
 * computes incremental deltas and turns them into GPUI `PinchEvent`s.
 */
export class PinchPlugin extends AsyncPluginBase {
  readonly id = "ohos.pinch";
  readonly requires: BridgeContextRequirement[] = ["ui-context"];
  private readonly mountKey = "ohos.pinch.overlay";
  private overlayNode?: BuilderNode<[PinchHandler]>;

  override onInstall(context: BridgePluginHookContext): void {
    try {
      const handler = new PinchHandler(context);
      const node = new BuilderNode<[PinchHandler]>(context.getUIContext());
      node.build(pinchBuilder, handler);
      const frameNode = node.getFrameNode();
      if (frameNode === null) {
        throw new Error("ohos.pinch: failed to build overlay FrameNode");
      }
      context.appendChild(this.mountKey, frameNode, (): void => {
        try {
          frameNode.dispose();
        } catch {}
      });
      this.overlayNode = node;
    } catch (error) {
      console.error(`ohos.pinch: overlay attach failed: ${String(error)}`);
    }
  }

  override onDispose(_context: BridgePluginHookContext): void {
    this.overlayNode = undefined;
  }

  async invokeAsync(
    _action: string,
    _request: BridgeTypedValue,
    _context: BridgeCallContext,
  ): Promise<BridgeTypedValue> {
    throw new Error(`Unsupported ohos.pinch action '${_action}'`);
  }
}
