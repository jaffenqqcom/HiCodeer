import common from '@ohos.app.ability.common';
import fs from '@ohos.file.fs';
import fileUri from '@ohos.file.fileuri';
import fileShare from '@ohos.fileshare';
import picker from '@ohos.file.picker';
import hilog from '@ohos.hilog';

const DOMAIN: number = 0x0001;
const TAG: string = 'Setup';
/** Named route loaded by `windowStage.loadContentByName`. */
export const RouteName: string = 'Setup';
/** File in the HAP private files dir holding the chosen home directory path. */
const RECORD_FILE: string = 'custom_data_dir';
/** Subdirectory created under the user-picked root; becomes the whole data root. */
const HOME_DIR_NAME: string = 'HiCodeer';
/** Paths under this prefix live outside the sandbox and need a picker grant. */
const USER_PUBLIC_PATH_PREFIX: string = '/storage/Users/currentUser';
/** Only one directory is selected; the picker rejects a larger count. */
const MAX_SELECT_NUMBER: number = 1;
/** Start-window logo size in vp; mirrors the launch screen's centred icon. */
const SPLASH_ICON_SIZE: number = 120;
/** Gap between the logo and the picker button, in vp. */
const ICON_TO_BUTTON_GAP: number = 24;
/** Gap between the picker button and its hint text, in vp. */
const BUTTON_TO_TEXT_GAP: number = 12;

let pendingResolve: ((homeDirectory: string) => void) | null = null;

/** Resolves with the chosen home directory once the user picks a folder. */
export function awaitSelection(): Promise<string> {
  return new Promise<string>((resolve: (homeDirectory: string) => void): void => {
    pendingResolve = resolve;
  });
}

/** Reads, records and resolves the chosen home directory (`<picked root>/HiCodeer`). */
export class HomeDirectory {
  /** Absolute path of the record file inside the HAP private files dir. */
  static recordPath(context: common.UIAbilityContext): string {
    return `${context.filesDir}/${RECORD_FILE}`;
  }

  /** The home directory recorded by a previous launch, or '' when none is recorded. */
  static read(context: common.UIAbilityContext): string {
    const path = HomeDirectory.recordPath(context);
    try {
      if (!fs.accessSync(path)) {
        return '';
      }
      return fs.readTextSync(path).trim();
    } catch (error) {
      hilog.error(DOMAIN, TAG, 'read %{public}s failed: %{public}s', path, String(error));
      return '';
    }
  }

  /** Records the home directory so the next launch finds it. Idempotent. */
  static write(context: common.UIAbilityContext, homeDirectory: string): void {
    const path = HomeDirectory.recordPath(context);
    try {
      const file = fs.openSync(path, fs.OpenMode.READ_WRITE | fs.OpenMode.CREATE | fs.OpenMode.TRUNC);
      fs.writeSync(file.fd, homeDirectory);
      fs.closeSync(file);
    } catch (error) {
      hilog.error(DOMAIN, TAG, 'write %{public}s failed: %{public}s', path, String(error));
    }
  }

  /**
   * Returns the recorded `<root>/HiCodeer` when it is still usable, else ''.
   * Re-activates the persisted picker grant first, mirroring the Rust side.
   */
  static async resolveExisting(context: common.UIAbilityContext): Promise<string> {
    const recorded = HomeDirectory.read(context);
    if (recorded === '') {
      return '';
    }
    if (!recorded.startsWith(USER_PUBLIC_PATH_PREFIX)) {
      hilog.warn(DOMAIN, TAG, 'home directory outside user-public prefix: %{public}s', recorded);
      return recorded;
    }
    try {
      const uri = fileUri.getUriFromPath(recorded);
      await fileShare.activatePermission([{
        uri: uri,
        operationMode: fileShare.OperationMode.READ_MODE | fileShare.OperationMode.WRITE_MODE,
      }]);
    } catch (error) {
      hilog.warn(DOMAIN, TAG, 'activatePermission failed: %{public}s', String(error));
    }
    // statSync throws when the path is gone, so a directory the user deleted is
    // rejected here and the setup page is shown again instead of being reused.
    try {
      const info = fs.statSync(recorded);
      if (!info.isDirectory()) {
        hilog.warn(DOMAIN, TAG, 'home directory is not a directory: %{public}s', recorded);
        return '';
      }
      return recorded;
    } catch (error) {
      hilog.warn(DOMAIN, TAG, 'home directory not accessible: %{public}s', String(error));
      return '';
    }
  }

  /** Creates `<root>/HiCodeer`, persists the grant, records it, and returns it. */
  static accept(root: string, uri: string, context: common.UIAbilityContext): string {
    const homeDirectory = `${root}/${HOME_DIR_NAME}`;
    try {
      fs.mkdirSync(homeDirectory, true);
    } catch (error) {
      hilog.warn(DOMAIN, TAG, 'mkdir %{public}s: %{public}s', homeDirectory, String(error));
    }
    try {
      fileShare.persistPermission([{
        uri: uri,
        operationMode: fileShare.OperationMode.READ_MODE | fileShare.OperationMode.WRITE_MODE,
      }]);
    } catch (error) {
      hilog.error(DOMAIN, TAG, 'persistPermission failed: %{public}s', String(error));
    }
    HomeDirectory.write(context, homeDirectory);
    return homeDirectory;
  }
}

@Entry({ routeName: RouteName })
@Component
struct Setup {
  private async pick(): Promise<void> {
    const context = getContext(this) as common.UIAbilityContext;
    const selectOptions = new picker.DocumentSelectOptions();
    selectOptions.maxSelectNumber = MAX_SELECT_NUMBER;
    // Folder mode: the home directory is a directory, not a file.
    selectOptions.selectMode = picker.DocumentSelectMode.FOLDER;
    const documentPicker = new picker.DocumentViewPicker(context);
    let uris: string[] = [];
    try {
      uris = await documentPicker.select(selectOptions);
    } catch (error) {
      // A cancelled picker rejects or resolves empty; stay on this page either way.
      hilog.warn(DOMAIN, TAG, 'directory picker returned no selection: %{public}s', String(error));
      return;
    }
    if (uris.length === 0) {
      return;
    }
    const uri = uris[0];
    const root = new fileUri.FileUri(uri).path ?? '';
    if (root === '') {
      hilog.error(DOMAIN, TAG, 'cannot resolve a local path from uri %{public}s', uri);
      return;
    }
    const homeDirectory = HomeDirectory.accept(root, uri, context);
    const resolve = pendingResolve;
    pendingResolve = null;
    if (resolve !== null) {
      resolve(homeDirectory);
    }
  }

  build() {
    Column() {
      // Same look as the launch screen: the start-window logo over the start
      // window background colour (which is dark/light aware via resources).
      Image($r('app.media.foreground'))
        .width(SPLASH_ICON_SIZE)
        .height(SPLASH_ICON_SIZE)
        .objectFit(ImageFit.Contain)
      Button($r('app.string.set_home_directory_button'))
        .margin({ top: ICON_TO_BUTTON_GAP })
        .onClick((): void => {
          this.pick();
        })
      Text($r('app.string.set_home_directory_hint'))
        .margin({ top: BUTTON_TO_TEXT_GAP })
    }
    .width('100%')
    .height('100%')
    .justifyContent(FlexAlign.Center)
    .backgroundColor($r('app.color.start_window_background'))
  }
}
