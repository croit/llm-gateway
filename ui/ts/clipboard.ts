// Copy-to-clipboard helper invoked from server-rendered buttons via
// `data-on:click="window.uiCopy(el)"`. The button carries
// `data-copy-target="#some-selector"`; we read `.textContent` of the
// target and write it to the clipboard, then surface a toast.
//
// Why textContent of a referenced element instead of e.g. a
// `data-clipboard` attribute on the button? The only caller is the
// minted-token banner — the freshly-generated secret only lives in
// the DOM inside the visible `<pre>`, so reading it back from there
// keeps the value duplicated zero times.

const uiCopy = async (btn: HTMLElement): Promise<void> => {
    const selector = btn.dataset.copyTarget;
    if (!selector) return;
    const target = document.querySelector(selector);
    if (!target) return;
    const text = target.textContent ?? '';
    try {
        await navigator.clipboard.writeText(text);
    } catch (err) {
        window.pushToast('error', `Couldn't copy: ${err}`);
        return;
    }
    window.pushToast('success', 'Copied to clipboard.');
};

window.uiCopy = uiCopy;
