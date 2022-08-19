import { ActionIcon, Checkbox, CheckIcon, Grid, Progress } from "@mantine/core";
import { PauseIcon, StopIcon } from "@radix-ui/react-icons";

import "./TList.scss";
import musicNote from "../../assets/musical-note.png";
import { FC, useEffect, useState } from "react";

interface ITListProp {
  id: string;
  fileName: string;
  percentDone: number;
  speed: number;
  totalSize: number;
  type: string;
}

const TList: FC<ITListProp> = ({
  fileName,
  percentDone,
  speed,
  totalSize,
  type,
  id,
}) => {
  const [percentProg, setPercentProg] = useState(0);

  useEffect(() => {
    // Listen to fake download event
    window.AwesomeEvent.listen(`time_elapsed_${id}`, (data) => {
      let t = JSON.stringify(data);
      setPercentProg(Number(t));
    });
  }, []);

  return (
    <Grid className="tlist_grid" columns={24}>
      <Grid.Col span={4} className="tlist_col">
        <Checkbox className="tlist_checkbox"></Checkbox>
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
            <p className="tfile_perc">{percentProg + "%" ?? "69%"}</p>
          </div>
          <div className="tfile_status">
            <Progress value={percentProg} />
          </div>
          <div className="tfile_info">
            <p className="tinfo_speed">{speed ?? "1.69MB/s"}</p>
            <p className="tinfo_size">{totalSize ?? "301MB"}</p>
          </div>
        </div>
      </Grid.Col>
      <Grid.Col span={4} className="tlist_col tfile_single_action">
        <ActionIcon color="dark">
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
