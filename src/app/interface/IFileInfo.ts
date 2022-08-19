export interface IFileInfoState {
  name: string;
  size: number;
  type: string;
  setFileInfo: (fileInfo: IFileInfo) => void;
}

export interface IFileInfo {
  id: string;
  name: string;
  size: number;
  type: string;
}

export interface IFileInfoRust{
  name: string,
  size: number,
}