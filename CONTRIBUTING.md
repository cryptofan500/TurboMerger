# Contributing to TurboMerger

Thank you for your interest in contributing to TurboMerger!

## Development Setup

1. Fork and clone the repository:
   ```bash
   git clone https://github.com/YOUR_USERNAME/turbomerger.git
   cd turbomerger
   ```

2. Install dependencies with uv:
   ```bash
   uv sync --all-groups
   ```

3. Run tests to verify setup:
   ```bash
   uv run pytest
   ```

## Code Style

We use ruff for linting and formatting:

```bash
# Check linting
uv run ruff check src tests

# Auto-fix issues
uv run ruff check --fix src tests

# Format code
uv run ruff format src tests
```

## Testing

All changes must pass tests:

```bash
# Run all tests
uv run pytest

# Run with coverage
uv run pytest --cov=turbomerger

# Run specific test file
uv run pytest tests/test_detector.py
```

## Pull Request Process

1. Create a feature branch from `main`:
   ```bash
   git checkout -b feature/your-feature-name
   ```

2. Make your changes with clear commit messages

3. Ensure tests pass and add new tests for new functionality

4. Update documentation if needed

5. Open a PR against `main` with a clear description

## Reporting Issues

When reporting bugs, please include:

- Python version (`python --version`)
- Operating system and version
- Steps to reproduce
- Expected vs actual behavior
- Error messages/tracebacks

## Feature Requests

Feature requests are welcome! Please open an issue describing:

- The problem you're trying to solve
- Your proposed solution
- Any alternatives you've considered

## Code of Conduct

Be respectful and constructive. We're all here to build great software together.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
