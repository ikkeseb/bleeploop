# Security

BleepLoop is a local desktop app with no accounts and no server. Its one network use is the update
check: the installed app asks GitHub whether a newer release exists and downloads it when you update
from Help. What can hurt you is what it loads. Third-party CLAP, VST3 and VST2 plugins are native code that
runs with your privileges, so only install plugins you trust. Imported session archives are the
other input.

Please report vulnerabilities privately through GitHub's **Report a vulnerability** button on the
repository's Security tab, not in a public issue. A fix ships in the next release, which the
installed app offers from Help.
