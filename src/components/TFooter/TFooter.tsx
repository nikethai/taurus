import { ActionIcon } from "@mantine/core";
import {
  CheckIcon,
  DownloadIcon,
  TriangleDownIcon,
  TriangleUpIcon,
  TrashIcon,
  GearIcon
} from "@radix-ui/react-icons";
import "./TFooter.scss";

const TFooter = () => {
  return (
    <footer className="tfooter">
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
          <DownloadIcon width={24} height={24} />
        </ActionIcon>
        <ActionIcon>
          <CheckIcon width={24} height={24} />
        </ActionIcon>
        <ActionIcon>
          <TrashIcon width={24} height={24} />
        </ActionIcon>
        <ActionIcon>
          <GearIcon width={24} height={24} />
        </ActionIcon>
      </div>
    </footer>
  );
};

export { TFooter };
