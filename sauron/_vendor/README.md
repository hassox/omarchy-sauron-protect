# Vendored libraries

Sauron runs straight from the plugin folder with the system Python, so it
installs nothing. The two protocol libraries it needs are copied here
verbatim from their PyPI wheels. Both are pure Python and sans-I/O: they parse
and produce bytes, and `sauron/net.py` moves those bytes over connections whose
certificate it has already checked.

| Library | Version | License | Wheel SHA-256 |
|---|---|---|---|
| [websockets](https://github.com/python-websockets/websockets) | 17.4 | BSD-3-Clause (`websockets/LICENSE`) | `c6bd2c9781915592b209160d0f017d4c7c82ad53075e6b0c24d2a89299e7f9e9` |
| [h11](https://github.com/python-hyper/h11) | 0.16.0 | MIT (`h11/LICENSE.txt`) | `63cf8bbe7522de3bf65932fda1d9c2772064ffb3dae62d55932da54b31cb6c86` |

Left out, nothing else changed:

- `websockets/speedups.c` and `speedups.pyi`: websockets then uses its
  pure-Python frame masking.
- `websockets/{asyncio,sync,trio}/router.py`: server-side routing that Sauron
  never imports. Their docs say to install
  werkzeug with pip, which the marketplace scanner reads as package management.

## Updating

1. Download the new `py3-none-any` wheel from PyPI and check its SHA-256
   against the one PyPI lists.
2. Replace the package directory here with the wheel's, drop the files listed
   above, and copy the license from the wheel's `.dist-info`.
3. Update the table above, run the tests, and say why in the commit.
