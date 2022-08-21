import create from "zustand";
import produce from "immer";
import { devtools, persist } from "zustand/middleware";
import { IListFileState, ISelectedListFileState } from "../interface/IListFile";
import { IFileInfo } from "../interface/IFileInfo";
import customStorage from "../util/customStorage";

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
      { name: "list-file", getStorage: () => customStorage }
    )
  )
);

const useSelectedListFileStore = create<ISelectedListFileState>()(
  devtools((set) => ({
    selectedListFile: [],
    setSelectListFile: (id: string) => {
      set(
        produce((state) => {
          if (state.selectedListFile.includes(id)) {
            state.selectedListFile = state.selectedListFile.filter(
              (item: string) => item !== id
            );
          } else {
            state.selectedListFile.push(id);
          }
        })
      );
    },
    getIsHavingUnresumable: () => {
      const listFile = useListFileStore.getState().listFile;
      const haveUnresumableItem = listFile.some((item) => !item.isResumable);
      return haveUnresumableItem;
    },
  }))
);

export { useListFileStore, useSelectedListFileStore };
