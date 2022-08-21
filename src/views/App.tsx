import { useEffect, useRef, useState } from "react";
import { AppShell, Button, Header, Modal, TextInput } from "@mantine/core";
import { invoke } from "@tauri-apps/api/tauri";
import { open } from "@tauri-apps/api/dialog";
import { readDir } from "@tauri-apps/api/fs";
import { appDir, downloadDir } from "@tauri-apps/api/path";
import short from "short-uuid";

import "./App.scss";
import { THeader } from "../components/THeader/THeader";
import { TFooter } from "../components/TFooter/TFooter";
import { TList } from "../components/TListInfo/TList";
import { TITLE } from "../app/util/constant";
import { TModal } from "../components/TModal/TModal";
import {
  useListFileStore,
  useSelectedListFileStore,
} from "../app/model/listFile";
import { IFileInfo, IFileInfoRust } from "../app/interface/IFileInfo";
import { formatBytes } from "../app/util/bytesFormat";

function App() {
  const [isAddModalOpened, setIsAddModalOpened] = useState(false);
  const [isDownloadModalOpened, setIsDownloadModalOpened] = useState(false);
  const [isGettingLink, setIsGettingLink] = useState(false);

  // Rust and Promise don't have cancellation mechanism, so we need to use this flag to stop the fetch
  // ref is synchronous
  const isCancel = useRef(false);

  const [downloadLink, setDownloadLink] = useState("");
  const [fileName, setFileName] = useState("");
  const [savePath, setSavePath] = useState("");
  const [fileSize, setFileSize] = useState("");
  const [availableSpace, setAvailableSpace] = useState("");
  const [contentLength, setContentLength] = useState("");

  const [isPathWritable, setIsPathWritable] = useState(false);
  const [isResumable, setIsResumable] = useState(false);


  const fileList = useListFileStore((state) => state.listFile);
  const setFileList = useListFileStore((state) => state.setListFile);

  const setSelectListFile = useSelectedListFileStore(
    (state) => state.setSelectListFile
  );

  const getDownloadInfo = async () => {
    setIsGettingLink(true);
    if (!downloadLink || downloadLink.length === 0) {
      return;
    }
    const fileInfo = await invoke<IFileInfoRust>("get_download_info", {
      link: downloadLink,
    });

    const defaultPath = await downloadDir();
    setSavePath(defaultPath);
    getAvailableSpace(defaultPath);
    checkPathPermission(defaultPath);

    setFileName(fileInfo.name);
    setFileSize(formatBytes(Number(fileInfo.size)));
    setContentLength(fileInfo.size);
    setIsResumable(fileInfo.isResumable);
    setIsGettingLink(false);
  };

  const cancelGetInfo = () => {
    invoke("cancel_get_download_info").catch((err) => console.log(err));
    setIsAddModalOpened(false);
    isCancel.current = true;
    setFileName("");
    setSavePath("");
  };

  const getSavePath = async () => {
    const selected = await open({
      directory: true,
      multiple: false,
      defaultPath: await appDir(),
      title: "Chọn thư mục lưu",
    });
    if (!selected) return;

    if (selected.length > 0) {
      if (!Array.isArray(selected)) {
        setSavePath(selected);
        getAvailableSpace(selected);
        checkPathPermission(selected);
      }
    }
  };

  const getAvailableSpace = async (selected: string) => {
    const availableSpace = await invoke<number>("get_disk_info", {
      path: selected,
    });
    setAvailableSpace(formatBytes(availableSpace));
  };

  const checkPathPermission = async (path: string) => {
    const result = await invoke<boolean>("check_permission", {
      path: path,
    });
    setIsPathWritable(result);
  };

  const downloadFile = async () => {
    if (!downloadLink || downloadLink.length === 0) {
      return;
    }
    if (!savePath || savePath.length === 0) {
      return;
    }

    const id = short.generate();

    setFileList({
      id,
      name: fileName,
      size: fileSize,
      contentLength,
      type: "video",
      isResumable: isResumable,
    });

    // Fake download
    // invoke("rp_time_elapsed", { id }).catch((err) => console.log(err));

    invoke("download_file", {
      id,
      link: downloadLink,
      savePath,
      fileName,
      contentLength,
    }).catch((err) => console.log(err));

    setIsDownloadModalOpened(false);
  };

  const selectFile = async (id: string) => {
    console.log(id);

    setSelectListFile(id);
  };

  // use to reset the state when the request is cancelled
  useEffect(() => {
    let timer1 = setTimeout(() => (isCancel.current = false), 1000);

    // this will clear Timeout
    // when component unmount like in willComponentUnmount
    // and is cancel will be reset to false
    return () => {
      clearTimeout(timer1);
    };
  }, [isCancel.current]);

  return (
    <AppShell
      className="material-theme"
      padding={5}
      header={<THeader onAdd={() => setIsAddModalOpened(true)} />}
      footer={<TFooter />}
    >
      <div className="tlist_display">
        {fileList.length > 0 ? (
          fileList.map((fileInfo, i) => (
            <TList
              id={fileInfo.id}
              fileName={fileInfo.name}
              type={fileInfo.type}
              totalSize={fileInfo.size}
              contentLength={fileInfo.contentLength}
              isResumable={fileInfo.isResumable}
              speed={1}
              setSelectFile={selectFile}
              key={i}
            />
          ))
        ) : (
          <div className="no_file">Chưa có tập tin nào</div>
        )}
      </div>
      {/*  AddModal */}
      <TModal
        isModalOpened={isAddModalOpened}
        title={TITLE.ADD}
        setModalOpened={(isOpened) => setIsAddModalOpened(isOpened)}
      >
        <TextInput
          onChange={(e) => setDownloadLink(e.target.value)}
          label="Đường dẫn"
          placeholder="Nhập đường dẫn..."
        />
        <div className="tmodal">
          <p className="tadd_error">Some kind of error</p>
          <div className="tadd_buttons">
            <Button
              onClick={async () => {
                await getDownloadInfo();
                if (!isCancel.current) {
                  setIsAddModalOpened(false);
                  setIsDownloadModalOpened(true);
                }
              }}
              loading={isGettingLink}
              variant="outline"
            >
              Thêm
            </Button>
            <Button onClick={cancelGetInfo} variant="outline">
              Huỷ
            </Button>
          </div>
        </div>
      </TModal>
      {/* endregion */}
      {/* DownloadModal */}
      <TModal
        isModalOpened={isDownloadModalOpened}
        title={TITLE.DOWNLOAD}
        setModalOpened={(isOpened) => setIsDownloadModalOpened(isOpened)}
      >
        <div className="tdownload_file_info">
          <div className="tdownload_file_location">
            <TextInput
              value={savePath}
              onChange={(e) => {
                // setSavePath(e.target.value);
              }}
              className="tdownload_input"
              label="Nơi lưu"
              placeholder="Nhập nơi bạn muốn lưu..."
            />
            <Button onClick={getSavePath} variant="subtle">
              Chọn chỗ lưu
            </Button>
          </div>
          <div>
            <TextInput
              value={fileName}
              onChange={(e) => {
                // setFileName(e.target.value);
              }}
              label="Tên tập tin"
              placeholder="Nhập tên..."
            />
            <p className="tdownload_info">
              File: {fileSize} (Còn trống: {availableSpace})
            </p>
          </div>
        </div>
        <div className="tmodal">
          {isPathWritable ? (
            <div />
          ) : (
            <p className="tdownload_permission">
              Không có quyền truy cập thư mục
            </p>
          )}
          <div className="tadd_buttons">
            <Button
              disabled={!isPathWritable}
              onClick={downloadFile}
              variant="outline"
            >
              Tải
            </Button>
            <Button
              onClick={() => {
                setFileName("");
                setSavePath("");
                setIsDownloadModalOpened(false);
              }}
              variant="outline"
            >
              Huỷ
            </Button>
          </div>
        </div>
      </TModal>
      {/* endregion */}
    </AppShell>
  );
}

export default App;
