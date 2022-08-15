import { useRef, useState } from "react";
import { AppShell, Header } from "@mantine/core";

import reactLogo from "./assets/react.svg";
import "./App.scss";
import { THeader } from "../components/THeader/THeader";
import { TFooter } from "../components/TFooter/TFooter";
import { TList } from "../components/TListInfo/TList";

function App() {
  return (
    <AppShell padding={5} header={<THeader />} footer={<TFooter />}>
      <div className="tlist_display">
        {[...Array(2)].map((_, i) => (
          <TList />
        ))}
      </div>
    </AppShell>
  );
}

export default App;
