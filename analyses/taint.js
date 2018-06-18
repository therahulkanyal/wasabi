// Author: Michael Pradel

/*
 * Simple taint analysis that considers explicit flows only
 * (i.e., no flows caused by control flow dependencies, but only data flow).
 * TODO work-in-progress ...
 */

(function() {

    console.log("=========================");
    console.log("Starting taint analysis");

    Array.prototype.peek = function() {
        return this[this.length - 1];
    };

    /*
     * Mirror program state; track taints instead of actual value
     * TODO: Move memory tracking into reusable analysis component
     */
    const stack = [{
        blocks:[], // the value stack for each function contains substacks for each block
        locals:[]
    }];

    function values() {
        return stack.peek().blocks.peek();
    }

    const memory = [];
    const globals = [];

    /*
     * Taint policy: sources and sink
     * TODO: use function names instead of hard-coded indices
     */
    const sourceFctIdx = 1;
    const sinkFctIdx = 2;

    function Taint() {
        this.label = 0; // can hold any kind of more complex label; for now, just 0 (not tained) and 1 (tainted)
    }

    Taint.prototype.toString = function() {
        return "taint-" + this.label;
    };

    function join(taint1, taint2) {
        const resultTaint = new Taint();
        if (taint1.label == 1 || taint2.label == 1)
            resultTaint.label = 1;
        return resultTaint;
    }

    Wasabi.analysis = {
        if_(location, condition) {
            values().pop();
        },

        br(location, target) {
            stack.peek().blocks.pop();
        },

        br_if(location, conditionalTarget, condition) {
            values().pop();
            if (condition) {
                stack.peek().blocks.pop();
            }
        },

        br_table(location, table, defaultTarget, tableIdx) {
            values().pop();
            stack.peek().blocks.pop();
        },

        begin(location, type) {
            // TODO if type is "function", set locals to parameter values of function?
            stack.peek().blocks.push([]);
        },

        end(location, type, beginLocation) {
            const [result] = stack.peek().blocks.pop();
            if (result !== undefined) {
                values().push(result);
            }
        },

        drop(location, value) {
            values().pop()
        },

        select(location, cond, first, second) {
            values().pop();
            values().pop();
            values().pop();
        },

        call_pre(location, targetFunc, args, indirectTableIdx) {
            if (indirectTableIdx !== undefined) {
                values().pop();
            }
            for (const arg of args) {
                const taint = values().pop();
                if (targetFunc == sourceFctIdx) {
                    taint.label = 1;
                    console.log("Source: Marking value as tainted -- " + taint);
                }
                if (targetFunc == sinkFctIdx && taint.label == 1) {
                    console.log("Tainted value reached sink at ", location);
                }
            }
            stack.push({
                blocks:[],
                locals:[],
            });
        },

        call_post(location, values) {
            stack.pop();
            for (const val of values) {
                values().push(val);
            }
        },

        return_(location, values) {
            // TODO how does it influence the stack? Is this already handled by end_function?
        },

        const_(location, value) {
            console.log("New taint at ", location);
            values().push(new Taint());
        },

        unary(location, op, input, result) {
            const taint = values().pop();
            const taintResult = new Taint();
            taintResult.label = taint.label;
            values().push(taintResult);
        },

        binary(location, op, first, second, result) {
            const taint1 = values().pop();
            const taint2 = values().pop();
            const taintResult = join(taint1, taint2);
            values().push(taintResult);
        },

        load(location, op, memarg, value) {
            values().pop();
            const effectiveAddr = memarg.addr + memarg.offset;
            const taint = memory[effectiveAddr];
            console.log("Memory load from address " + effectiveAddr + " with taint " + taint);
            values().push(taint);
        },

        store(location, op, memarg, value) {
            const taint = values().pop();
            values().pop();
            const effectiveAddr = memarg.addr + memarg.offset;
            console.log("Memory store to address " + effectiveAddr + " with taint " + taint);
            memory[effectiveAddr] = taint;
        },

        memory_size(location, currentSizePages) {
            values().push(currentSizePages);
        },

        memory_grow(location, byPages, previousSizePages) {
            values().pop();
            values().push(previousSizePages);
        },

        local(location, op, localIndex, value) {
            switch (op) {
                case "set_local": {
                    const taint = values().pop();
                    stack.peek().locals[localIndex] = taint;
                    console.log("Setting local variable with taint " + taint);
                    return;
                }
                case "tee_local": {
                    const taint = values().peek();
                    stack.peek().locals[localIndex] = taint;
                    return;
                }
                case "get_local": {
                    const taint = stack.peek().locals[localIndex];
                    values().push(taint);
                    console.log("Getting local variable with taint " + taint);
                    return;
                }
            }
        },

        global(location, op, globalIndex, value) {
            switch (op) {
                case "set_global":
                    const jsValue = values().pop();
                    globals[globalIndex] = value;
                    return;
                case "get_global":
                    values().push(value);
                    return;
            }
        },
    };

})();
