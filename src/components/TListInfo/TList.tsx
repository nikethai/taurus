import { ActionIcon, Checkbox, CheckIcon, Grid } from "@mantine/core";
import { CheckboxIcon } from "@radix-ui/react-icons";

import "./TList.scss";

const TList = () => {
  return (
    <Grid className="tlist_grid" columns={24}>
      <Grid.Col span={3} className="tlist_col">
        <Checkbox></Checkbox>
        
      </Grid.Col>
      <Grid.Col span={18} className="tlist_col">
        <ActionIcon>
          <CheckboxIcon height={20} width={30} />
        </ActionIcon>
      </Grid.Col>
      <Grid.Col span={3} className="tlist_col">
        <ActionIcon>
          <CheckboxIcon height={20} width={30} />
        </ActionIcon>
      </Grid.Col>
    </Grid>
  );
};

export { TList };
