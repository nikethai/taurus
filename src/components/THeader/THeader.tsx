import {
  ActionIcon,
  Grid,
  Header,
  TextInput,
  Tooltip,
} from "@mantine/core";
import {
  PlusIcon,
  PlayIcon,
  PauseIcon,
  TrashIcon,
} from "@radix-ui/react-icons";
import { TOOLTIP } from "../../app/util/constant";
import "./THeader.scss";

const THeader = () => {
  return (
    <Header height={60} title="Mantine">
      <Grid className="theader_grid" columns={12} align="center">
        <Grid.Col className="theader_col" span={1}>
          <ActionIcon>
            <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.ADD}>
              <PlusIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
        </Grid.Col>
        <Grid.Col span={9}>
          <TextInput placeholder="Tìm kiếm..." />
        </Grid.Col>
        <Grid.Col className="theader_col theader_col--between" span={2}>
          <ActionIcon>
            <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.RESUME}>
              <PlayIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
          <ActionIcon>
            <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.PAUSE}>
              <PauseIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
          <ActionIcon>
            <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.DELETE}>
              <TrashIcon height={30} width={30} />
            </Tooltip>
          </ActionIcon>
        </Grid.Col>
      </Grid>
    </Header>
  );
};

export { THeader };
