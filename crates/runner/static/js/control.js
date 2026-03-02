(function initControlModule() {
  function createControlState() {
    return {
      busyByUser: {},
    };
  }

  function isUserBusy(state, userId) {
    return Boolean(state && state.busyByUser && state.busyByUser[userId]);
  }

  function setUserBusy(state, userId, busy) {
    if (!state.busyByUser) {
      state.busyByUser = {};
    }
    state.busyByUser[userId] = Boolean(busy);
  }

  window.OxydraControl = {
    createControlState,
    isUserBusy,
    setUserBusy,
  };
}());
