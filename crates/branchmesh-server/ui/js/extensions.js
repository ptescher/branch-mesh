// UI extensions. An application that serves this UI (with its own routes
// next to the operator API) can replace this file to add views. Each entry:
//
//   {
//     views: { name: { mount(container, params) -> unmount? } },
//     nav: [{ view: 'name', label: 'Shown in the top bar' }],
//     styles: ['/my.css'],            // extra stylesheets
//     eventKinds: ['my_event'],       // extra SSE event names for subscribe()
//     async init() {                  // optional; may restrict the session:
//       return { views: ['name'], home: 'name', badge: 'role label' };
//     },
//   }
//
// A restricted session (non-operator caller) only sees the listed views and
// skips the operator stats / live event stream.
export default [];
