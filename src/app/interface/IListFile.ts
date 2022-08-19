import { IFileInfo } from "./IFileInfo";

export interface IListFileState {
    listFile: IFileInfo[];
    setListFile: (listFile: IFileInfo) => void;
}
