import create from "zustand";
import produce from "immer";
import { devtools, persist } from "zustand/middleware";
import { IListFileState } from "../interface/IListFile";
import { IFileInfo } from "../interface/IFileInfo";

const useListFileStore = create<IListFileState>()(
  devtools(
    persist(
      (set) => ({
        listFile: [],
        setListFile: (listFile: IFileInfo) => {
          set(
            produce((state) => {
              state.listFile.push(listFile);
            })
          );
        },
      }),
      { name: "list-file" }
    )
  )
);

export { useListFileStore };
