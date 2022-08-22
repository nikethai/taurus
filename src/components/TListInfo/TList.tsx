import { ActionIcon, Checkbox, CheckIcon, Grid, Progress } from "@mantine/core";
import { PauseIcon, StopIcon } from "@radix-ui/react-icons";

import "./TList.scss";
import musicNote from "../../assets/musical-note.png";
import { FC, useEffect, useRef, useState } from "react";
import { formatBytes } from "../../app/util/bytesFormat";
import { flushSync } from "react-dom";

interface ITListProp {
  id: string;
  fileName: string;
  speed: number;
  totalSize: string;
  contentLength: string;
  type: string;
  isResumable: boolean;
  setSelectFile: (file: string) => void;
}

const TList: FC<ITListProp> = ({
  fileName,
  totalSize,
  contentLength,
  type,
  id,
  isResumable,
  setSelectFile,
}) => {
  const [percentProg, setPercentProg] = useState(0);
  const [speed, setSpeed] = useState(0);

  const chunkForSpeed = useRef(0);

  let timer: number | null = null;
  let incrChunk = 0;

  const updateSpeed = (time = 3) => {
    if (speed === 0) {
      setSpeed(chunkForSpeed.current);
    } else {
      flushSync(() =>
        setSpeed(
          (prevSpeed) => prevSpeed + (chunkForSpeed.current - prevSpeed) / time
        )
      );
    }
  };

  useEffect(() => {
    // Listen to download event
    // load the percentage
    window.AwesomeEvent.listen(`download_progress_${id}`, (data) => {
      // for some reason it's double the value
      // because of the FUCKING DOUBLE USEEFFECT
      let realChunk = data.chunkPerSec ;
      incrChunk += realChunk;

      //
      chunkForSpeed.current += realChunk;

      setPercentProg(Number((incrChunk / Number(contentLength)) * 100));
    });

    window.AwesomeEvent.listen(`download_status_${id}`, (data) => {
      if (data.status === "started") {
        timer = setInterval(() => {
          updateSpeed();
          chunkForSpeed.current = 0;
        }, 1000);
      } else {
        if (timer) {
          clearInterval(timer);
          timer = null;
        }
      }
    });
  }, []);

  //Fck react

  return (
    <Grid className="tlist_grid" columns={24}>
      <Grid.Col span={4} className="tlist_col">
        <Checkbox
          onChange={(e) => setSelectFile(id)}
          className="tlist_checkbox"
        ></Checkbox>
        <div className="tlist_flex_grow_center">
          <img
            className="tlist_file_type_img"
            src={musicNote}
            alt="file-type-img"
          />
        </div>
      </Grid.Col>
      <Grid.Col span={16} className="tlist_col">
        <div className="tlist_file_status">
          <div className="tfile_name_perc">
            <p className="tfile_name">{fileName ?? "File name"}</p>
            <p className="tfile_perc">
              {percentProg.toFixed(2) + "%" ?? "69%"}
            </p>
          </div>
          <div className="tfile_status">
            <Progress value={percentProg} />
          </div>
          <div className="tfile_info">
            <p className="tinfo_speed">
              {formatBytes(speed) + "/s" ?? "1.69MB/s"}
            </p>
            <p className="tinfo_size">{totalSize ?? "301MB"}</p>
          </div>
        </div>
      </Grid.Col>
      <Grid.Col span={4} className="tlist_col tfile_single_action">
        <ActionIcon disabled={!isResumable} color="dark">
          <PauseIcon height={20} width={20} />
        </ActionIcon>
        <ActionIcon color="dark">
          <StopIcon height={20} width={20} />
        </ActionIcon>
      </Grid.Col>
    </Grid>
  );
};

export { TList };
