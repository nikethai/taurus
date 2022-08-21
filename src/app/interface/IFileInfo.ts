export interface IFileInfoState {
  name: string;
  size: string;
  type: string;
  setFileInfo: (fileInfo: IFileInfo) => void;
}

export interface IFileInfo {
  id: string;
  name: string;
  size: string;
  type: string;
}

export interface IFileInfoRust{
  name: string,
  size: string,
}