import create from "zustand";
import produce from "immer";
import { devtools, persist } from "zustand/middleware";

import { IFileInfo, IFileInfoState } from "../interface/IFileInfo";

const useFileInfoStore = create<IFileInfoState>()(
  devtools(
    persist(
      (set) => ({
        name: "",
        size: 0,
        type: "",
        setFileInfo: (fileInfo: IFileInfo) => {
          set(
            produce((state) => {
              state.name = fileInfo.name;
              state.size = fileInfo.size;
              state.type = fileInfo.type;
            })
          );
        },
      }),
      { name: "file-info" }
    )
  )
);

export { useFileInfoStore };
