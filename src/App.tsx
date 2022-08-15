import { useRef, useState } from "react";
import { AppShell, Header } from "@mantine/core";

import reactLogo from "./assets/react.svg";
import "./App.scss";
import { THeader } from "./components/THeader/THeader";
import { TFooter } from "./components/TFooter/TFooter";
import { TList } from "./components/TListInfo/TList";

function App() {
  return (
    <AppShell header={<THeader />} footer={<TFooter />}>
      <TList></TList>
    </AppShell>
  );
}

export default App;
