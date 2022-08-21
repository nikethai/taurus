import { IFileInfo } from "./IFileInfo";

export interface IListFileState {
    listFile: IFileInfo[];
    setListFile: (listFile: IFileInfo) => void;
}

export interface ISelectedListFileState {
    selectedListFile: string[];
    setSelectListFile: (id: string) => void;
}

