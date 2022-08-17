import { MantineThemeOverride } from "@mantine/core";

const mantineTheme: MantineThemeOverride = {
    colors:{
        'mat-primary': ["#d8fdff", "#acf0ff", "#7de6fe", "#4cdafb", "#20d0f9", "#06b6df", "#008eaf", "#00657e", "#003e4d", "#00161f"],
        'mat-secondary': ["#e9f3ff", "#cdd9e5", "#b1becf", "#93a5b8", "#758ba3", "#5c718a", "#47586c", "#313f4e", "#1b2632", "#000f17"],
        'mat-tertiary': ["#ffecf1", "#e6cfd6", "#ceb1bb", "#b9939f", "#a47484", "#8b5b6a", "#6d4753", "#4f323b", "#311c23", "#18060e"],
        'mat-error': ["#ffe4e4", "#fcb9b9", "#f48d8c", "#ed605e", "#e73432", "#cd1a18", "#a11212", "#730b0b", "#470505", "#1f0000"],
        'mat-neutral':["#f1f1fc", "#d8d6df", "#bebcc3", "#a4a2aa", "#8a8791", "#716d77", "#58555d", "#3f3d44", "#27232a", "#120912"],
    },
    primaryColor: 'mat-primary',
    fontFamily: 'Noto Sans ,sans-serif'
};

export { mantineTheme };
