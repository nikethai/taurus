import { Modal } from "@mantine/core";
import { FC } from "react";

export interface ITModalProps {
  isModalOpened: boolean;
  setModalOpened: (isOpened: boolean) => void;
  children: React.ReactNode;
  title: String;
}

const TModal: FC<ITModalProps> = ({
  setModalOpened,
  isModalOpened,
  children,
  title
}) => {
  return (
    <Modal
      opened={isModalOpened}
      onClose={() => setModalOpened(false)}
      title={title}
      centered
      withCloseButton={false}
      overlayOpacity={0.35}
      overlayBlur={2}
      transition="pop"
      transitionDuration={200}
      transitionTimingFunction="ease"
      size="75%"
    >
      {children}
    </Modal>
  );
};

export { TModal };
