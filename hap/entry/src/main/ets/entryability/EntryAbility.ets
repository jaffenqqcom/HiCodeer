import 'libzcoder.so';
import { BridgePlugin, LazyPlugin, NativeAbility } from '@ohos-rs/ability';
import { CursorPlugin } from '@ohos-rs/ability-plugin-cursor';
import { FilesPlugin } from '@ohos-rs/ability-plugin-files';
import { PinchPlugin } from '@ohos-rs/ability-plugin-pinch';
import { FileDropPlugin } from '@ohos-rs/ability-plugin-filedrop';

// EntryAbility：鸿蒙 Ability 生命周期入口。
// moduleName 与 native 库名一致（libzcoder.so），NativeAbility 基类负责把窗口/surface/IME/
// 生命周期事件经 NAPI 路由到 Rust 侧（crates/zed 的 #[ability] openharmony_app 入口）。
// defaultPage=true：让 NativeAbility 基类加载默认页。
// loadMode='sync'：同步加载 native 模块，避免默认页动态 import 导致黑屏（无错误日志）。
// bridgePlugins：注册 ArkTS 侧能力插件，与 Rust 侧 app.register_plugin 声明一一对应。
export default class EntryAbility extends NativeAbility {
  public moduleName: string = 'zcoder';
  public defaultPage: boolean = true;
  public loadMode: 'sync' | 'async' = 'sync';
  public bridgePlugins: LazyPlugin[] = [
    new LazyPlugin((): BridgePlugin => new CursorPlugin()),
    new LazyPlugin((): BridgePlugin => new FilesPlugin()),
    new LazyPlugin((): BridgePlugin => new PinchPlugin()),
    new LazyPlugin((): BridgePlugin => new FileDropPlugin()),
  ];
}
