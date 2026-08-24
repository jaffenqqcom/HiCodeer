import common from "@ohos.app.ability.common";
import deviceInfo from "@ohos.deviceInfo";
import picker from "@ohos.file.picker";
import {
  BridgeCallContext,
  BridgeTypedValue,
  AsyncPluginBase,
  BridgeContextRequirement,
} from "@ohos-rs/ability";

const DIALOG_OPTIONS_TYPE = "ohos.files.DialogOptions";
const DIALOG_RESPONSE_TYPE = "ohos.files.DialogResponse";

const FOLDER_SELECTION_SYSCAP = "SystemCapability.FileManagement.UserFileService.FolderSelection";

interface FileDialogFilter {
  name?: string | null;
  pattern?: string | null;
}

interface FileDialogOptions {
  dialogType: string;
  allowMany: boolean;
  defaultLocation?: string | null;
  filters: FileDialogFilter[];
}

interface DocumentViewPickerWithSelectedIndex {
  getSelectedIndex(): number;
}

// API 26+ only: allowsMulFolderSelection is absent on older SDKs, so access it through
// a narrow interface to keep the package compatible with compatibleSdkVersion = 23.
interface DocumentSelectOptionsWithMulFolder {
  allowsMulFolderSelection: boolean;
}

class FileDialogResponse {
  readonly files: string[];
  readonly filter: number;

  constructor(files: string[], filter: number) {
    this.files = files;
    this.filter = filter;
  }
}

function parseDialogOptions(payload: BridgeTypedValue): FileDialogOptions {
  if (payload.typeName !== DIALOG_OPTIONS_TYPE || typeof payload.value !== "object") {
    throw new Error(`ohos.files.file-dialog requires bridge type ${DIALOG_OPTIONS_TYPE}`);
  }
  const request = payload.value as FileDialogOptions;
  if (
    request.dialogType !== "open-file" &&
    request.dialogType !== "save-file" &&
    request.dialogType !== "open-folder"
  ) {
    throw new Error(`file-dialog.dialogType '${request.dialogType}' is unsupported`);
  }
  if (typeof request.filters !== "object") {
    throw new Error("file-dialog.filters must be an array");
  }
  return request;
}

/**
 * Picker-specific filter grammar is an internal ArkTS concern: structured `{ name, pattern }`
 * groups arrive through the bridge and are converted to the `name|*.ext` choice strings here.
 * No string-encoded filter grammar crosses the bridge.
 */
function pickerSuffixFilters(filters: FileDialogFilter[]): string[] | undefined {
  const choices: string[] = [];
  for (const filter of filters) {
    const pattern = filter.pattern || "*";
    const name = (filter.name || "Files").split("|").join(" ");
    if (pattern === "*") {
      choices.push(`${name}|.*`);
      continue;
    }
    const suffixes: string[] = [];
    const parts = pattern.split(";");
    for (const part of parts) {
      const trimmed = part.trim();
      if (trimmed.length > 0) {
        suffixes.push(trimmed.startsWith(".") ? trimmed : `.${trimmed}`);
      }
    }
    if (suffixes.length > 0) {
      choices.push(`${name}|${suffixes.join(",")}`);
    }
  }
  return choices.length > 0 ? choices : undefined;
}

function pickerLocation(location: string | undefined | null): string | undefined {
  return location && location.indexOf("://") > 0 ? location : undefined;
}

function supportsFolderSelection(): boolean {
  // Folder selection is gated by the FolderSelection system capability (2-in-1 and
  // tablet devices), not by a specific device type.
  return canIUse(FOLDER_SELECTION_SYSCAP);
}

async function showFileDialog(
  context: common.UIAbilityContext,
  options: FileDialogOptions,
): Promise<FileDialogResponse> {
  const documentPicker = new picker.DocumentViewPicker(context);
  const location = pickerLocation(options.defaultLocation);
  const suffixFilters = pickerSuffixFilters(options.filters || []);

  if (options.dialogType === "save-file") {
    const saveOptions = new picker.DocumentSaveOptions();
    saveOptions.defaultFilePathUri = location;
    saveOptions.fileSuffixChoices = suffixFilters;
    const files = await documentPicker.save(saveOptions);
    let filter = -1;
    if (deviceInfo.sdkApiVersion >= 14 && supportsFolderSelection()) {
      // Keep the package compatible with API 12 while using the API 14 method only after the
      // runtime capability check. The narrow adapter prevents the static checker from rejecting
      // an otherwise guarded call solely because compatibleSdkVersion is lower than 14.
      filter = (documentPicker as DocumentViewPickerWithSelectedIndex).getSelectedIndex();
    }
    return new FileDialogResponse(files, filter);
  }

  const selectOptions = new picker.DocumentSelectOptions();
  selectOptions.defaultFilePathUri = location;
  selectOptions.maxSelectNumber = options.allowMany ? 500 : 1;
  selectOptions.fileSuffixFilters = suffixFilters;
  if (options.dialogType === "open-folder") {
    // Bypass the canIUse capability gate: canIUse() can be more conservative than the actual
    // select(FOLDER) call, so let the system decide (same approach as warp's PickDirectory).
    selectOptions.selectMode = picker.DocumentSelectMode.FOLDER;
    selectOptions.maxSelectNumber = options.allowMany ? 500 : 1;
    selectOptions.fileSuffixFilters = undefined;
    if (deviceInfo.sdkApiVersion >= 26 && options.allowMany) {
      // API 26+ requires allowsMulFolderSelection for multi-folder selection; on
      // API 24/25 maxSelectNumber alone is sufficient (directory limit removed since API 23).
      (selectOptions as DocumentSelectOptionsWithMulFolder).allowsMulFolderSelection = true;
    }
  }
  try {
    return new FileDialogResponse(await documentPicker.select(selectOptions), -1);
  } catch (error) {
    console.error(`documentPicker.select failed: ${JSON.stringify(error)}`);
    throw new Error(JSON.stringify(error));
  }
}

export class FilesPlugin extends AsyncPluginBase {
  readonly id = "ohos.files";
  readonly requires: BridgeContextRequirement[] = ["ability"];

  async invokeAsync(
    action: string,
    payload: BridgeTypedValue,
    context: BridgeCallContext,
  ): Promise<BridgeTypedValue> {
    if (action !== "file-dialog") {
      throw new Error(`Unsupported ohos.files action '${action}'`);
    }
    const options = parseDialogOptions(payload);
    const response = await showFileDialog(context.abilityContext, options);
    if (!context.isActive()) {
      throw new Error("Ability was destroyed while the file dialog was pending");
    }
    return { typeName: DIALOG_RESPONSE_TYPE, value: response };
  }
}
