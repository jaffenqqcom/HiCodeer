import { unifiedDataChannel } from "@kit.ArkData";
import { BuilderNode } from "@ohos.arkui.node";
import {
  AsyncPluginBase,
  BridgeCallContext,
  BridgeContextRequirement,
  BridgePluginHookContext,
  BridgeTypedValue,
} from "@ohos-rs/ability";

const DROP_FILES_TYPE = "ohos.filedrop.DropFilesData";
const DRAG_MOVE_TYPE = "ohos.filedrop.DragMoveData";
const DROP_ACK_TYPE = "ohos.filedrop.DropAck";

const DRAG_ENTER_EVENT = "drag-enter";
const DRAG_MOVE_EVENT = "drag-move";
const DROP_FILES_EVENT = "drop-files";

interface DragMoveData {
  positionX: number;
  positionY: number;
}

interface DropFilesData {
  files: string[];
  positionX: number;
  positionY: number;
}

/**
 * Forwards dropped file URIs to Rust through the synchronous main-thread bridge.
 * The overlay stays hit-test transparent so pointer input passes through to the
 * underlying XComponent; only drop drags are captured here.
 */
class DropHandler {
  private readonly context: BridgePluginHookContext;

  constructor(context: BridgePluginHookContext) {
    this.context = context;
  }

  onDragEnter(): void {
    // Establish the GPUI drag state (active_drag) as soon as the drag enters, so
    // the invisible drop target becomes visible and registers its drop listener.
    try {
      this.context.invokeNativeSync(DRAG_ENTER_EVENT, DROP_FILES_TYPE, DROP_ACK_TYPE, { files: [], positionX: 0, positionY: 0 } as ESObject);
    } catch (error) {
      console.error(`ohos.filedrop drag-enter failed: ${String(error)}`);
    }
  }

  onDragMove(x: number, y: number): void {
    // Continuous drag position, mirroring X11's XdndPosition -> Pending; keeps
    // the hover hit test on the now-visible drop target.
    const payload: DragMoveData = { positionX: x, positionY: y };
    try {
      this.context.invokeNativeSync(DRAG_MOVE_EVENT, DRAG_MOVE_TYPE, DROP_ACK_TYPE, payload as ESObject);
    } catch (error) {
      console.error(`ohos.filedrop drag-move failed: ${String(error)}`);
    }
  }

  onDrop(event: DragEvent): void {
    try {
      const data = event.getData();
      const records = data.getRecords();
      const files: string[] = [];
      for (const record of records) {
        if (record instanceof unifiedDataChannel.File) {
          const uri = record.uri;
          if (uri.length > 0) {
            files.push(uri);
          }
        }
      }
      if (files.length === 0) {
        console.warn("ohos.filedrop: onDrop found no file records");
        return;
      }
      const payload: DropFilesData = {
        files,
        positionX: event.getWindowX(),
        positionY: event.getWindowY(),
      };
      this.context.invokeNativeSync(DROP_FILES_EVENT, DROP_FILES_TYPE, DROP_ACK_TYPE, payload as ESObject);
    } catch (error) {
      console.error(`ohos.filedrop: onDrop failed: ${String(error)}`);
    }
  }
}

@Builder
function DropOverlay(handler: DropHandler) {
  Stack()
    .width("100%")
    .height("100%")
    .hitTestBehavior(HitTestMode.Transparent)
    .onDragEnter(() => {
      handler.onDragEnter();
    })
    .onDragMove((event: DragEvent) => {
      handler.onDragMove(event.getWindowX(), event.getWindowY());
    })
    .onDrop((event: DragEvent) => {
      handler.onDrop(event);
    })
}

const dropBuilder = wrapBuilder<[DropHandler]>(DropOverlay);

/**
 * External file drop capability plugin.
 *
 * Attaches a transparent full-screen drop-target overlay and forwards dropped
 * file URIs to Rust (ohos.filedrop) via invokeNativeSync. Rust queues them and
 * gpui_ohos turns them into GPUI `FileDropEvent`s.
 */
export class FileDropPlugin extends AsyncPluginBase {
  readonly id = "ohos.filedrop";
  readonly requires: BridgeContextRequirement[] = ["ui-context"];
  private readonly mountKey = "ohos.filedrop.overlay";
  private overlayNode?: BuilderNode<[DropHandler]>;

  override onInstall(context: BridgePluginHookContext): void {
    try {
      const handler = new DropHandler(context);
      const node = new BuilderNode<[DropHandler]>(context.getUIContext());
      node.build(dropBuilder, handler);
      const frameNode = node.getFrameNode();
      if (frameNode === null) {
        throw new Error("ohos.filedrop: failed to build overlay FrameNode");
      }
      context.appendChild(this.mountKey, frameNode, (): void => {
        try {
          frameNode.dispose();
        } catch {}
      });
      this.overlayNode = node;
    } catch (error) {
      console.error(`ohos.filedrop: overlay attach failed: ${String(error)}`);
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
    throw new Error(`Unsupported ohos.filedrop action '${_action}'`);
  }
}
