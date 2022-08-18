import { useEffect, useRef, useState } from "react";
import { AppShell, Button, Header, Modal, TextInput } from "@mantine/core";
import { invoke } from "@tauri-apps/api/tauri";
import { open } from "@tauri-apps/api/dialog";
import { appDir } from "@tauri-apps/api/path";

import "./App.scss";
import { THeader } from "../components/THeader/THeader";
import { TFooter } from "../components/TFooter/TFooter";
import { TList } from "../components/TListInfo/TList";
import { TITLE } from "../app/util/constant";
import { TModal } from "../components/TModal/TModal";

function App() {
  const [isAddModalOpened, setIsAddModalOpened] = useState(false);
  const [isDownloadModalOpened, setIsDownloadModalOpened] = useState(false);
  const [isGettingLink, setIsGettingLink] = useState(false);

  // Rust and Promise don't have cancellation mechanism, so we need to use this flag to stop the fetch
  // ref is synchronous
  const isCancel = useRef(false);

  const [downloadLink, setDownloadLink] = useState("");
  const [fileName, setFileName] = useState("");

  const getDownloadInfo = async () => {
    setIsGettingLink(true);
    if (!downloadLink || downloadLink.length === 0) {
      return;
    }
    const name = await invoke("get_download_info", {
      link: downloadLink,
    }).catch((err) => console.log(err));

    setFileName(name as string);
    setIsGettingLink(false);
  };

  const cancelGetInfo = () => {
    invoke("cancel_get_download_info").catch((err) => console.log(err));
    setIsAddModalOpened(false);
    isCancel.current = true;
    setFileName("");
  };

  const getSavePath = async () => {
    const selected = await open({
      directory: true,
      multiple: false,
      defaultPath: await appDir(),
    });

    console.log(selected);
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
        {[...Array(2)].map((_, i) => (
          <TList key={i} />
        ))}
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
                // await getDownloadInfo();
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
              label="Tên tập tin"
              placeholder="Nhập tên..."
            />
            <p className="tdownload_info">File: 6,9GB (Còn trống: 100GB)</p>
          </div>
        </div>
        <div className="tmodal">
          <p className="tdownload_permission">Không có quyền truy cập</p>
          <div className="tadd_buttons">
            <Button onClick={getDownloadInfo} variant="outline">
              Thêm
            </Button>
            <Button
              onClick={() => {
                setFileName("");
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
