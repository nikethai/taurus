import {
  ActionIcon,
  Col,
  createStyles,
  Grid,
  Header,
  TextInput,
} from "@mantine/core";
import {
  PlusIcon,
  PlayIcon,
  PauseIcon,
  TrashIcon,
  Cross2Icon,
} from "@radix-ui/react-icons";
import "./THeader.scss";

const THeader = () => {
  return (
    <Header height={60} title="Mantine">
      <Grid className="theader_grid" columns={12} align="center">
        <Grid.Col className="theader_col" span={1}>
          <ActionIcon>
            <PlusIcon height={30} width={30} />
          </ActionIcon>
        </Grid.Col>
        <Grid.Col span={9}>
          <TextInput placeholder="Tìm kiếm..." />
        </Grid.Col>
        <Grid.Col className="theader_col theader_col--between" span={2}>
          <ActionIcon>
            <PlayIcon height={30} width={30} />
          </ActionIcon>
          <ActionIcon>
            <PauseIcon height={30} width={30} />
          </ActionIcon>
          <ActionIcon>
            <Cross2Icon height={30} width={30} />
          </ActionIcon>
        </Grid.Col>
      </Grid>
    </Header>
  );
};

export { THeader };
