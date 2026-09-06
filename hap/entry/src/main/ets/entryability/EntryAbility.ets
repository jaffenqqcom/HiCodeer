import 'libzcoder.so';
import { BridgePlugin, LazyPlugin, NativeAbility } from '@ohos-rs/ability';
import { CursorPlugin } from '@ohos-rs/ability-plugin-cursor';
import { FilesPlugin } from '@ohos-rs/ability-plugin-files';
import { PinchPlugin } from '@ohos-rs/ability-plugin-pinch';
import { FileDropPlugin } from '@ohos-rs/ability-plugin-filedrop';
import { OpenWithPlugin } from '@ohos-rs/ability-plugin-openwith';
import { ImePlugin } from '@ohos-rs/ability-plugin-ime';
import { UrlPlugin } from '@ohos-rs/ability-plugin-url';
import { Want, AbilityConstant } from '@kit.AbilityKit';

// EntryAbility：鸿蒙 Ability 生命周期入口。
// moduleName 与 native 库名一致（libzcoder.so），NativeAbility 基类负责把窗口/surface/IME/
// 生命周期事件经 NAPI 路由到 Rust 侧（crates/zed 的 #[ability] openharmony_app 入口）。
// defaultPage=true：让 NativeAbility 基类加载默认页。
// loadMode='sync'：同步加载 native 模块，避免默认页动态 import 导致黑屏（无错误日志）。
// bridgePlugins：注册 ArkTS 侧能力插件，与 Rust 侧 app.register_plugin 声明一一对应。
// 单一共享的 openWith 实例：onCreate/onNewWant 触发后需引用它，且插件安装前可能先收到 want。
const openWith = new OpenWithPlugin();

export default class EntryAbility extends NativeAbility {
  public moduleName: string = 'zcoder';
  public defaultPage: boolean = true;
  public loadMode: 'sync' | 'async' = 'sync';
  public bridgePlugins: LazyPlugin[] = [
    new LazyPlugin((): BridgePlugin => new CursorPlugin()),
    new LazyPlugin((): BridgePlugin => new FilesPlugin()),
    new LazyPlugin((): BridgePlugin => new PinchPlugin()),
    new LazyPlugin((): BridgePlugin => new FileDropPlugin()),
    new LazyPlugin((): BridgePlugin => openWith),
    new LazyPlugin((): BridgePlugin => new ImePlugin()),
    new LazyPlugin((): BridgePlugin => new UrlPlugin()),
  ];

  // 冷启动：系统带 want（含要打开的文件 URI）启动。必须先让基类完成会话初始化，
  // 再把 want 交给 openWith 处理（若插件尚未 onInstall，会在内部暂存）。
  onCreate(want: Want, launchParam: AbilityConstant.LaunchParam): void {
    super.onCreate(want, launchParam);
    openWith.openFromWant(want);
  }

  // 已运行时再次收到打开请求（如文件管理器“打开方式”选 zcoder）。
  onNewWant(want: Want, launchParam: AbilityConstant.LaunchParam): void {
    super.onNewWant(want, launchParam);
    openWith.openFromWant(want);
  }
}
