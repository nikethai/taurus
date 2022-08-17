import { useRef, useState } from "react";
import { AppShell, Button, Header, Modal, TextInput } from "@mantine/core";
import { invoke } from "@tauri-apps/api/tauri";

import "./App.scss";
import { THeader } from "../components/THeader/THeader";
import { TFooter } from "../components/TFooter/TFooter";
import { TList } from "../components/TListInfo/TList";
import { TITLE } from "../app/util/constant";
import { TModal } from "../components/TModal/TModal";

function App() {
  const [isAddModalOpened, setIsAddModalOpened] = useState(false);
  const [isDownloadModalOpened, setIsDownloadModalOpened] = useState(false);

  const getDownloadInfo = () => {
    invoke("my_custom_command").then((res) => console.log(res));
  };

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
      <AddModal
        isAddModalOpened={isAddModalOpened}
        setIsAddModalOpened={setIsAddModalOpened}
        setIsDownloadModalOpened={setIsDownloadModalOpened}
      />
      <DownloadModal
        isDownloadModalOpened={isDownloadModalOpened}
        setIsDownloadModalOpened={setIsDownloadModalOpened}
        getDownloadInfo={getDownloadInfo}
      />
    </AppShell>
  );
}

interface IAddModal {
  isAddModalOpened: boolean;
  setIsAddModalOpened: (isOpened: boolean) => void;
  setIsDownloadModalOpened: (isOpened: boolean) => void;
}
function AddModal({
  isAddModalOpened,
  setIsAddModalOpened,
  setIsDownloadModalOpened,
}: IAddModal) {
  return (
    <TModal
      isModalOpened={isAddModalOpened}
      title={TITLE.ADD}
      setModalOpened={(isOpened) => setIsAddModalOpened(isOpened)}
    >
      <TextInput label="Đường dẫn" placeholder="Nhập đường dẫn..." />
      <div className="tmodal">
        <p className="tadd-error">Some kind of error</p>
        <div className="tadd-buttons">
          <Button
            onClick={() => {
              setIsDownloadModalOpened(true);
              setIsAddModalOpened(false);
            }}
            variant="outline"
          >
            Thêm
          </Button>
          <Button onClick={() => setIsAddModalOpened(false)} variant="outline">
            Huỷ
          </Button>
        </div>
      </div>
    </TModal>
  );
}

interface IDownloadModal {
  isDownloadModalOpened: boolean;
  setIsDownloadModalOpened: (isOpened: boolean) => void;
  getDownloadInfo: () => void;
}
function DownloadModal({
  isDownloadModalOpened,
  setIsDownloadModalOpened,
  getDownloadInfo,
}: IDownloadModal) {
  return (
    <TModal
      isModalOpened={isDownloadModalOpened}
      title={TITLE.ADD}
      setModalOpened={(isOpened) => setIsDownloadModalOpened(isOpened)}
    >
      <div className="tdownload-file-info">
        <TextInput label="Nơi lưu" placeholder="Nhập nơi bạn muốn lưu..." />
        <div>
          <TextInput label="Tên tập tin" placeholder="Nhập tên..." />
          <p className="tdownload-info">File: 6,9GB (Còn trống: 100GB)</p>
        </div>
      </div>
      <div className="tmodal">
        <p className="tdownload-permission">Không có quyền truy cập</p>
        <div className="tadd-buttons">
          <Button onClick={getDownloadInfo} variant="outline">
            Thêm
          </Button>
          <Button
            onClick={() => setIsDownloadModalOpened(false)}
            variant="outline"
          >
            Huỷ
          </Button>
        </div>
      </div>
    </TModal>
  );
}

export default App;
