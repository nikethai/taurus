import { ActionIcon, Footer, Tooltip } from "@mantine/core";
import {
  CheckIcon,
  DownloadIcon,
  TriangleDownIcon,
  TriangleUpIcon,
  GearIcon,
  CounterClockwiseClockIcon,
} from "@radix-ui/react-icons";
import { TOOLTIP } from "../../app/util/constant";
import "./TFooter.scss";

const TFooter = () => {
  return (
    <Footer height={55} className="tfooter">
      <div className="tfooter-align-center tfooter-align-center-left">
        <span className="tfooter_download_display">
          <ActionIcon>
            <TriangleDownIcon width={20} height={20} />
          </ActionIcon>
          <p>69.420KB/s</p>
        </span>
        <span className="tfooter_download_display">
          <ActionIcon>
            <TriangleUpIcon width={20} height={20} />
          </ActionIcon>
          <p>69.420KB/s</p>
        </span>
      </div>
      <div className="tfooter-align-center tfooter-align-center-right nav-gap">
        <ActionIcon>
          <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.PAGE.CURRENT}>
            <DownloadIcon width={24} height={24} />
          </Tooltip>
        </ActionIcon>
        <ActionIcon>
          <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.PAGE.COMPLETE}>
            <CheckIcon width={24} height={24} />
          </Tooltip>
        </ActionIcon>
        <ActionIcon>
          <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.PAGE.PAUSE}>
            <CounterClockwiseClockIcon width={24} height={24} />
          </Tooltip>
        </ActionIcon>
        <ActionIcon>
          <Tooltip transition="pop" transitionDuration={300} label={TOOLTIP.PAGE.SETTING}>
            <GearIcon width={24} height={24} />
          </Tooltip>
        </ActionIcon>
      </div>
    </Footer>
  );
};

export { TFooter };
