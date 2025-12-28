"""Entry point for TurboMerger application."""

import sys


def main() -> None:
    """Launch TurboMerger GUI."""
    from turbomerger.gui import TurboMergerApp

    app = TurboMergerApp()
    app.mainloop()


if __name__ == "__main__":
    sys.exit(main() or 0)
