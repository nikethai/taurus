import React from "react";
import ReactDOM from "react-dom/client";
import { MantineProvider } from "@mantine/core";

import App from "./views/App";
import "./index.css";
import "./styles.css";
import { mantineTheme } from "./app/util/mantineTheme";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <MantineProvider theme={mantineTheme}>
      <App />
    </MantineProvider>
  </React.StrictMode>
);
