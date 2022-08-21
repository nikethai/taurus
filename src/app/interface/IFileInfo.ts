export interface IFileInfoState {
  name: string;
  size: string;
  type: string;
  isResumable: boolean;
  setFileInfo: (fileInfo: IFileInfo) => void;
}

export interface IFileInfo {
  id: string;
  name: string;
  size: string;
  contentLength: string;
  type: string;
  isResumable: boolean;
}

export interface IFileInfoRust{
  name: string,
  size: string,
  isResumable: boolean;
}