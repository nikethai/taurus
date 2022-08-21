import { ActionIcon, Grid, Header, TextInput, Tooltip } from "@mantine/core";
import {
  PlusIcon,
  PlayIcon,
  PauseIcon,
  TrashIcon,
} from "@radix-ui/react-icons";
import { FC, useEffect, useState } from "react";
import { useSelectedListFileStore } from "../../app/model/listFile";
import { TOOLTIP } from "../../app/util/constant";
import "./THeader.scss";

export interface ITHeaderProps {
  onAdd: () => void;
}

const THeader: FC<ITHeaderProps> = ({ onAdd }) => {
  const selectedListFiles = useSelectedListFileStore(
    (state) => state.selectedListFile
  );
  const [haveFile, setHaveFile] = useState(selectedListFiles.length > 0);

  useEffect(() => {
    setHaveFile(selectedListFiles.length > 0);
  }, [selectedListFiles]);

  return (
    <Header height={60} title="Taurus Downloader" className="theader_colour">
      <Grid className="theader_grid" columns={12} align="center">
        <Grid.Col
          style={{ justifyContent: "start" }}
          className="theader_col"
          span={1}
        >
          <Tooltip
            transition="pop"
            transitionDuration={300}
            label={TOOLTIP.ADD}
          >
            <ActionIcon
              sx={{
                marginLeft: 13,
              }}
              onClick={onAdd}
              color="dark"
            >
              <PlusIcon height={30} width={30} />
            </ActionIcon>
          </Tooltip>
        </Grid.Col>
        <Grid.Col span={9}>
          <TextInput className="theader_input" placeholder="Tìm kiếm..." />
        </Grid.Col>
        <Grid.Col className="theader_col theader_col--between" span={2}>
          <ActionIcon
            sx={{
              backgroundColor: "transparent",
            }}
            disabled={!haveFile}
            color="dark"
          >
            <Tooltip
              transition="pop"
              transitionDuration={300}
              label={TOOLTIP.RESUME}
            >
              <PlayIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
          <ActionIcon disabled={!haveFile} color="dark">
            <Tooltip
              transition="pop"
              transitionDuration={300}
              label={TOOLTIP.PAUSE}
            >
              <PauseIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
          <ActionIcon disabled={!haveFile} color="dark">
            <Tooltip
              transition="pop"
              transitionDuration={300}
              label={TOOLTIP.DELETE}
            >
              <TrashIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
        </Grid.Col>
      </Grid>
    </Header>
  );
};

export { THeader };
